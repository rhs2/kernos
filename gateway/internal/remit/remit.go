// Package remit parses and verifies the signed capability tokens of
// 03-REMIT. The seven checks are separate functions so the gateway can run
// them in the specified order with the connector's scope derivation between
// the fifth and the seventh, and so each refusal reason is testable on its
// own. The signature is verified over the exact decoded payload bytes; the
// payload is parsed only after the signature holds.
package remit

import (
	"context"
	"crypto/ed25519"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"strings"
	"time"

	"github.com/rhs2/kernos/gateway/connect"
)

// Prefix is the token format version this gateway understands.
const Prefix = "krt1"

// Refusal reasons, exactly the eight of 03-REMIT.
const (
	ReasonTokenMalformed   = "token_malformed"
	ReasonSignatureInvalid = "signature_invalid"
	ReasonExpired          = "remit_expired"
	ReasonNotYetValid      = "remit_not_yet_valid"
	ReasonRunMismatch      = "remit_run_mismatch"
	ReasonToolNotInRemit   = "tool_not_in_remit"
	ReasonScopeNotGranted  = "scope_not_granted"
	ReasonAutonomyTooLow   = "autonomy_too_low"
	autonomyObserve        = "observe"
	autonomyPropose        = "propose"
	autonomySupervised     = "supervised"
	autonomyAutonomous     = "autonomous"
)

// Refusal is a failed check: the reason code the response carries and a
// human detail.
type Refusal struct {
	Reason string `json:"reason"`
	Detail string `json:"detail"`
}

func refuse(reason, format string, args ...any) *Refusal {
	return &Refusal{Reason: reason, Detail: fmt.Sprintf(format, args...)}
}

// Spend is the remit's spend ceiling; the gateway carries it but does not
// enforce it (budgets are the kernel's job).
type Spend struct {
	Tokens float64 `json:"tokens"`
	USD    float64 `json:"usd"`
}

// Claims is the decoded payload.
type Claims struct {
	RID         string         `json:"rid"`
	Parent      string         `json:"parent"`
	Run         string         `json:"run"`
	Iss         string         `json:"iss"`
	Iat         float64        `json:"iat"`
	Nbf         float64        `json:"nbf"`
	Exp         float64        `json:"exp"`
	Tools       []string       `json:"tools"`
	Scopes      []string       `json:"scopes"`
	Grants      []string       `json:"grants"`
	Spend       Spend          `json:"spend"`
	Autonomy    string         `json:"autonomy"`
	PolicySet   []string       `json:"policy_set"`
	RequestedBy map[string]any `json:"requested_by"`
}

// Token is a parsed token. Claims is valid only after CheckSignature
// succeeded.
type Token struct {
	Raw       string
	Payload   []byte
	Signature []byte
	KeyID     string
	Claims    Claims
	decoded   bool
}

// Decoded reports whether the claims have been parsed (that is, whether the
// signature check passed).
func (t *Token) Decoded() bool { return t.decoded }

// RemitID returns the remit id when the claims are decoded, else "".
func (t *Token) RemitID() string {
	if t == nil || !t.decoded {
		return ""
	}
	return t.Claims.RID
}

var b64 = base64.RawURLEncoding.Strict()

// Parse performs the first check: prefix krt1, four parts, base64url without
// padding for the payload and the signature, a non-empty key id.
func Parse(raw string) (*Token, *Refusal) {
	if raw == "" {
		return nil, refuse(ReasonTokenMalformed, "remit_token is empty")
	}
	parts := strings.Split(raw, ".")
	if len(parts) != 4 {
		return nil, refuse(ReasonTokenMalformed, "token must have four dot-separated parts, got %d", len(parts))
	}
	if parts[0] != Prefix {
		return nil, refuse(ReasonTokenMalformed, "unknown token prefix %q", truncate(parts[0], 16))
	}
	if parts[1] == "" || parts[2] == "" || parts[3] == "" {
		return nil, refuse(ReasonTokenMalformed, "token has an empty part")
	}
	payload, err := b64.DecodeString(parts[1])
	if err != nil {
		return nil, refuse(ReasonTokenMalformed, "payload is not base64url without padding")
	}
	sig, err := b64.DecodeString(parts[2])
	if err != nil {
		return nil, refuse(ReasonTokenMalformed, "signature is not base64url without padding")
	}
	return &Token{Raw: raw, Payload: payload, Signature: sig, KeyID: parts[3]}, nil
}

// KeyResolver finds the public key for a key id. The boolean is false when
// the key is unknown.
type KeyResolver interface {
	PublicKey(ctx context.Context, keyID string) (ed25519.PublicKey, bool)
}

// Verifier runs the signature check against a key resolver.
type Verifier struct {
	Keys KeyResolver
}

// CheckSignature performs the second check and then decodes the payload. A
// payload that is not the JSON object of 03-REMIT is token_malformed.
func (v *Verifier) CheckSignature(ctx context.Context, tok *Token) *Refusal {
	if v == nil || v.Keys == nil {
		return refuse(ReasonSignatureInvalid, "no verification key is available")
	}
	pub, ok := v.Keys.PublicKey(ctx, tok.KeyID)
	if !ok || len(pub) != ed25519.PublicKeySize {
		return refuse(ReasonSignatureInvalid, "unknown key id %q", truncate(tok.KeyID, 64))
	}
	if len(tok.Signature) != ed25519.SignatureSize {
		return refuse(ReasonSignatureInvalid, "signature must be %d bytes, got %d", ed25519.SignatureSize, len(tok.Signature))
	}
	if !ed25519.Verify(pub, tok.Payload, tok.Signature) {
		return refuse(ReasonSignatureInvalid, "signature does not verify for key %q", truncate(tok.KeyID, 64))
	}
	return decodeClaims(tok)
}

func decodeClaims(tok *Token) *Refusal {
	var c Claims
	if err := json.Unmarshal(tok.Payload, &c); err != nil {
		return refuse(ReasonTokenMalformed, "payload is not a remit object: %v", err)
	}
	if c.Exp <= 0 {
		return refuse(ReasonTokenMalformed, "payload has no exp")
	}
	switch c.Autonomy {
	case autonomyObserve, autonomyPropose, autonomySupervised, autonomyAutonomous:
	default:
		return refuse(ReasonTokenMalformed, "unknown autonomy %q", truncate(c.Autonomy, 32))
	}
	if c.Tools == nil {
		c.Tools = []string{}
	}
	if c.Scopes == nil {
		c.Scopes = []string{}
	}
	tok.Claims = c
	tok.decoded = true
	return nil
}

// CheckTime performs the third check: nbf <= now < exp.
func CheckTime(tok *Token, now time.Time) *Refusal {
	n := float64(now.Unix())
	if n < tok.Claims.Nbf {
		return refuse(ReasonNotYetValid, "remit is valid from %s", time.Unix(int64(tok.Claims.Nbf), 0).UTC().Format(time.RFC3339))
	}
	if n >= tok.Claims.Exp {
		return refuse(ReasonExpired, "remit expired at %s", time.Unix(int64(tok.Claims.Exp), 0).UTC().Format(time.RFC3339))
	}
	return nil
}

// CheckRun performs the fourth check: a run-bound token only serves its run.
func CheckRun(tok *Token, runID string) *Refusal {
	if tok.Claims.Run != "" && tok.Claims.Run != runID {
		return refuse(ReasonRunMismatch, "remit is bound to run %s, call is for %q", tok.Claims.Run, runID)
	}
	return nil
}

// CheckTool performs the fifth check: the tool matches a tools pattern.
func CheckTool(tok *Token, toolID string) *Refusal {
	if !connect.MatchAny(tok.Claims.Tools, toolID) {
		return refuse(ReasonToolNotInRemit, "%s not matched by %s", toolID, formatList(tok.Claims.Tools))
	}
	return nil
}

// CheckScopes performs the sixth check: every derived scope is granted.
func CheckScopes(tok *Token, scopes []string) *Refusal {
	for _, s := range scopes {
		if !connect.MatchAny(tok.Claims.Scopes, s) {
			return refuse(ReasonScopeNotGranted, "%s not granted by %s", s, formatList(tok.Claims.Scopes))
		}
	}
	return nil
}

// CheckAutonomy performs the seventh check: observe and propose refuse
// every write tool (03-REMIT lists propose "as observe" at the gateway);
// supervised and autonomous allow writes and leave the gating to policy.
func CheckAutonomy(tok *Token, writes bool) *Refusal {
	if writes && AutonomyRank(tok.Claims.Autonomy) < AutonomyRank(autonomySupervised) {
		return refuse(ReasonAutonomyTooLow, "autonomy %s does not allow write tools", tok.Claims.Autonomy)
	}
	return nil
}

// AutonomyRank orders the levels: observe < propose < supervised <
// autonomous. Unknown levels rank -1.
func AutonomyRank(level string) int {
	switch level {
	case autonomyObserve:
		return 0
	case autonomyPropose:
		return 1
	case autonomySupervised:
		return 2
	case autonomyAutonomous:
		return 3
	}
	return -1
}

// Sign builds a token for the given payload bytes. Only the kernel mints
// remits in production; this exists for tests and local tooling.
func Sign(payload []byte, priv ed25519.PrivateKey, keyID string) string {
	sig := ed25519.Sign(priv, payload)
	return Prefix + "." + b64.EncodeToString(payload) + "." + b64.EncodeToString(sig) + "." + keyID
}

// ParsePublicKey decodes a public key given as base64url (with or without
// padding), standard base64 or hex, and checks it is 32 bytes.
func ParsePublicKey(s string) (ed25519.PublicKey, error) {
	s = strings.TrimSpace(s)
	if s == "" {
		return nil, fmt.Errorf("public key is empty")
	}
	decoders := []func(string) ([]byte, error){
		base64.RawURLEncoding.DecodeString,
		base64.URLEncoding.DecodeString,
		base64.RawStdEncoding.DecodeString,
		base64.StdEncoding.DecodeString,
		hex.DecodeString,
	}
	for _, dec := range decoders {
		if b, err := dec(s); err == nil && len(b) == ed25519.PublicKeySize {
			return ed25519.PublicKey(b), nil
		}
	}
	return nil, fmt.Errorf("public key must decode to %d bytes", ed25519.PublicKeySize)
}

func formatList(items []string) string {
	return "[" + strings.Join(items, ", ") + "]"
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "..."
}
