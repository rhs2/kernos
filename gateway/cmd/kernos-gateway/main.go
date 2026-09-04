// Command kernos-gateway is the Kernos gateway binary: it hosts connectors,
// verifies the remit on every tool call, keeps the idempotency store and
// runs the contract canaries. Configuration comes from --config gateway.json,
// KERNOS_GATEWAY_* and KERNOS_* environment variables, and the flags below,
// in that order of precedence.
package main

import (
	"context"
	"crypto/ed25519"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
	"github.com/rhs2/kernos/gateway/internal/canary"
	"github.com/rhs2/kernos/gateway/internal/config"
	_ "github.com/rhs2/kernos/gateway/internal/connectors/fs"
	_ "github.com/rhs2/kernos/gateway/internal/connectors/http"
	mcpconn "github.com/rhs2/kernos/gateway/internal/connectors/mcp"
	_ "github.com/rhs2/kernos/gateway/internal/connectors/sqlite"
	_ "github.com/rhs2/kernos/gateway/internal/connectors/testtools"
	"github.com/rhs2/kernos/gateway/internal/idem"
	"github.com/rhs2/kernos/gateway/internal/kernel"
	"github.com/rhs2/kernos/gateway/internal/remit"
	"github.com/rhs2/kernos/gateway/internal/server"
)

// Exit codes: 0 clean shutdown, 1 runtime failure, 2 configuration error.
const (
	exitOK      = 0
	exitRuntime = 1
	exitConfig  = 2
)

func main() {
	os.Exit(run(os.Args[1:], os.Stdout, os.Stderr))
}

func run(args []string, stdout, stderr io.Writer) int {
	flags := flag.NewFlagSet("kernos-gateway", flag.ContinueOnError)
	flags.SetOutput(stderr)
	listen := flags.String("listen", "", "address to listen on (default 127.0.0.1:7402)")
	configPath := flags.String("config", "", "path to gateway.json")
	dataDir := flags.String("data", "", "data directory for the idempotency store and repair requests (default ./gateway-data)")
	kernelURL := flags.String("kernel", "", "kernel base URL (default http://127.0.0.1:7401)")
	logFormat := flags.String("log", "", "log format: json or text (default json)")
	showVersion := flags.Bool("version", false, "print the version and exit")
	if err := flags.Parse(args); err != nil {
		return exitConfig
	}
	if *showVersion {
		fmt.Fprintln(stdout, "kernos-gateway", server.Version)
		return exitOK
	}
	cfg, secrets, err := config.Load(*configPath, config.EnvLookup)
	if err != nil {
		fmt.Fprintln(stderr, "kernos-gateway:", err)
		return exitConfig
	}
	if *listen != "" {
		cfg.Listen = *listen
	}
	if *dataDir != "" {
		cfg.DataDir = *dataDir
	}
	if *kernelURL != "" {
		cfg.KernelURL = *kernelURL
	}
	if *logFormat != "" {
		cfg.LogFormat = *logFormat
	}
	if err := config.Validate(cfg); err != nil {
		fmt.Fprintln(stderr, "kernos-gateway:", err)
		return exitConfig
	}
	logger := newLogger(cfg, secrets, stderr)
	mcpconn.Logger = logger

	if err := os.MkdirAll(cfg.DataDir, 0o755); err != nil {
		logger.Error("cannot create the data directory", "data_dir", cfg.DataDir, "error", err.Error())
		return exitConfig
	}
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGTERM, syscall.SIGINT)
	defer stop()

	kc := kernel.New(cfg.KernelURL, cfg.Token, 5*time.Second)
	var keys *remit.KeyStore
	if cfg.PublicKey != "" {
		pub, err := remit.ParsePublicKey(cfg.PublicKey)
		if err != nil {
			logger.Error("KERNOS_PUBLIC_KEY is invalid", "error", err.Error())
			return exitConfig
		}
		keys = remit.NewPinnedKeyStore(pub, logger)
		logger.Info("remit verification key pinned from configuration")
	} else {
		keys = remit.NewKeyStore(func(ctx context.Context) (string, ed25519.PublicKey, error) {
			k, err := kc.FetchKeys(ctx)
			if err != nil {
				return "", nil, err
			}
			pub, err := remit.ParsePublicKey(k.PublicKey)
			if err != nil {
				return "", nil, fmt.Errorf("kernel public key: %w", err)
			}
			return k.KeyID, pub, nil
		}, time.Hour, logger)
		keys.Start(ctx)
	}

	built, err := server.BuildConnectors(cfg, logger)
	if err != nil {
		logger.Error("connector configuration failed", "error", err.Error())
		return exitConfig
	}
	store, err := idem.Open(filepath.Join(cfg.DataDir, "idempotency.db"))
	if err != nil {
		logger.Error("idempotency store failed", "error", err.Error())
		return exitRuntime
	}
	mgr := canary.New(canary.Options{
		Interval:        time.Duration(cfg.Canary.IntervalSeconds * float64(time.Second)),
		QuarantineAfter: cfg.Canary.QuarantineAfter,
		AutoRelease:     cfg.Canary.AutoRelease,
		RepairDir:       filepath.Join(cfg.DataDir, "repairs"),
		Log:             logger,
	})
	srv, err := server.New(server.Options{
		Config:     cfg,
		Logger:     logger,
		Secrets:    secrets,
		Verifier:   &remit.Verifier{Keys: keys},
		Kernel:     kc,
		Idem:       store,
		Canary:     mgr,
		Connectors: built,
	})
	if err != nil {
		store.Close()
		logger.Error("server setup failed", "error", err.Error())
		return exitConfig
	}
	srv.Start(ctx)

	httpServer := &http.Server{
		Addr:              cfg.Listen,
		Handler:           srv.Handler(),
		ReadHeaderTimeout: 10 * time.Second,
		IdleTimeout:       2 * time.Minute,
	}
	errCh := make(chan error, 1)
	go func() {
		logger.Info("kernos-gateway listening",
			"listen", cfg.Listen, "kernel_url", cfg.KernelURL, "data_dir", cfg.DataDir,
			"connectors", len(built), "connector_types", connect.Types(), "test_tools", cfg.TestTools,
			"canary_interval_s", cfg.Canary.IntervalSeconds, "quarantine_after", cfg.Canary.QuarantineAfter,
			"auto_release", cfg.Canary.AutoRelease, "version", server.Version)
		errCh <- httpServer.ListenAndServe()
	}()
	code := exitOK
	select {
	case <-ctx.Done():
		logger.Info("shutdown signal received")
	case err := <-errCh:
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			logger.Error("http server failed", "error", err.Error())
			code = exitRuntime
		}
	}
	shutdownCtx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if err := httpServer.Shutdown(shutdownCtx); err != nil {
		logger.Warn("http shutdown did not finish cleanly", "error", err.Error())
		httpServer.Close()
	}
	if err := srv.Close(); err != nil {
		logger.Warn("server close reported an error", "error", err.Error())
	}
	logger.Info("kernos-gateway stopped")
	return code
}

func newLogger(cfg *config.Config, secrets *config.Secrets, stderr io.Writer) *slog.Logger {
	level := slog.LevelInfo
	switch cfg.LogLevel {
	case "debug":
		level = slog.LevelDebug
	case "warn":
		level = slog.LevelWarn
	case "error":
		level = slog.LevelError
	}
	out := secrets.Writer(stderr)
	opts := &slog.HandlerOptions{Level: level}
	if cfg.LogFormat == "text" {
		return slog.New(slog.NewTextHandler(out, opts))
	}
	return slog.New(slog.NewJSONHandler(out, opts))
}
