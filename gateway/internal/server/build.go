package server

import (
	"fmt"
	"log/slog"

	"github.com/rhs2/kernos/gateway/connect"
	"github.com/rhs2/kernos/gateway/internal/config"
	"github.com/rhs2/kernos/gateway/internal/connectors/testtools"
)

// BuildConnectors instantiates every connector of the configuration through
// the connect registry and works out its probe. A connector of type "test"
// is built only when test tools are enabled, and one named "test" is added
// automatically when they are.
func BuildConnectors(cfg *config.Config, log *slog.Logger) ([]Built, error) {
	if log == nil {
		log = slog.Default()
	}
	entries := append([]map[string]any{}, cfg.Connectors...)
	if cfg.TestTools {
		present := false
		for _, e := range entries {
			if e["name"] == testtools.DefaultName {
				present = true
			}
		}
		if !present {
			entries = append(entries, map[string]any{"name": testtools.DefaultName, "type": testtools.TypeName})
		}
	}
	var out []Built
	for i, entry := range entries {
		name, _ := entry["name"].(string)
		typ, _ := entry["type"].(string)
		if typ == testtools.TypeName && !cfg.TestTools {
			log.Warn("skipping test connector because KERNOS_GATEWAY_TEST_TOOLS is not set", "connector", name)
			continue
		}
		factory, ok := connect.Lookup(typ)
		if !ok {
			return nil, fmt.Errorf("connectors[%d] %q: unknown connector type %q (registered: %v)", i, name, typ, connect.Types())
		}
		conn, err := factory(entry)
		if err != nil {
			return nil, fmt.Errorf("connectors[%d] %q: %w", i, name, err)
		}
		if conn.Name() != name {
			return nil, fmt.Errorf("connectors[%d]: factory for %q returned a connector named %q", i, name, conn.Name())
		}
		b := Built{Connector: conn}
		if pd, ok := conn.(connect.ProbeDescriber); ok {
			b.Probe, b.HasProbe = pd.ProbeSpec()
		} else if tool, args, contract, has, err := connect.ProbeFromConfig(entry); err != nil {
			return nil, fmt.Errorf("connectors[%d] %q: %w", i, name, err)
		} else if has {
			b.Probe = connect.ProbeSpec{Tool: tool, Args: args}
			for _, spec := range conn.Tools() {
				if spec.ID == connect.ToolID(name, tool) {
					b.Probe.Contract = spec.Contract
				}
			}
			if contract != nil {
				b.Probe.Contract = *contract
			}
			b.HasProbe = true
		}
		log.Info("connector ready", "connector", name, "type", typ, "tools", len(conn.Tools()), "probe", b.HasProbe)
		out = append(out, b)
	}
	return out, nil
}
