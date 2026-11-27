package hashinbox

import (
	"encoding/json"
	"os"
	"reflect"
	"strings"
	"testing"
	"time"
)

// read decodes the vector file name into v.
func read(t *testing.T, name string, v any) {
	t.Helper()
	raw, err := os.ReadFile("../_golden/" + name)
	if err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(raw, v); err != nil {
		t.Fatal(err)
	}
}

// scopesText is text's space-separated scopes, as packages/scope parses
// them.
func scopesText(t *testing.T, text string) Scopes {
	t.Helper()
	scopes := Scopes{}
	for _, field := range strings.Fields(text) {
		scope, err := ParseScope(field)
		if err != nil {
			t.Fatalf("%q: %v", field, err)
		}
		scopes = append(scopes, scope)
	}
	return scopes
}

func TestScopeVectors(t *testing.T) {
	var v struct {
		Parse []struct {
			Text, Display string
		}
		Refuse []string
		Covers []struct {
			Ours, Theirs string
			Covers       bool
		}
		Escape []struct {
			ID, Text string
		}
		FirstUncovered []struct {
			Granted, Asked string
			First          *string `json:"first_uncovered"`
		} `json:"first_uncovered"`
		Sets []struct {
			Scopes, Display, Compressed string
			Packed                      map[string]ScopeGroup
		}
		Unpack []struct {
			Packed map[string]ScopeGroup
			Scopes *string
		}
		Expand struct {
			Depends map[string][]string
			Cases   []struct{ Scopes, Expanded string }
		}
	}
	read(t, "scope.json", &v)
	if len(v.Parse) == 0 || len(v.Refuse) == 0 || len(v.Covers) == 0 || len(v.Escape) == 0 ||
		len(v.FirstUncovered) == 0 || len(v.Sets) == 0 || len(v.Unpack) == 0 || len(v.Expand.Depends) == 0 || len(v.Expand.Cases) == 0 {
		t.Fatal("a vector section is empty or renamed")
	}
	for _, c := range v.Parse {
		scope, err := ParseScope(c.Text)
		if err != nil || scope.String() != c.Display {
			t.Errorf("parse %q: %v, %q, want %q", c.Text, err, scope, c.Display)
		}
	}
	for _, text := range v.Refuse {
		if _, err := ParseScope(text); err == nil {
			t.Errorf("refuse %q: parsed", text)
		}
	}
	for _, c := range v.Covers {
		ours, theirs := scopesText(t, c.Ours)[0], scopesText(t, c.Theirs)[0]
		if ours.Covers(theirs) != c.Covers {
			t.Errorf("%s covers %s: want %v", c.Ours, c.Theirs, c.Covers)
		}
	}
	for _, c := range v.Escape {
		if got := EscapeID(c.ID); got != c.Text {
			t.Errorf("escape %q: %q, want %q", c.ID, got, c.Text)
		}
	}
	for _, c := range v.FirstUncovered {
		first, found := scopesText(t, c.Granted).FirstUncovered(scopesText(t, c.Asked))
		if found != (c.First != nil) || (found && first.String() != *c.First) {
			t.Errorf("first uncovered of %q by %q: %v %q, want %v", c.Asked, c.Granted, found, first, c.First)
		}
	}
	for _, c := range v.Sets {
		scopes := scopesText(t, c.Scopes)
		if scopes.String() != c.Display {
			t.Errorf("display %q: %q", c.Scopes, scopes)
		}
		if got := scopes.compress().String(); got != c.Compressed {
			t.Errorf("compress %q: %q, want %q", c.Scopes, got, c.Compressed)
		}
		packed := scopes.Pack()
		if !reflect.DeepEqual(packed, c.Packed) {
			t.Errorf("pack %q: %v, want %v", c.Scopes, packed, c.Packed)
		}
		unpacked, err := ParseScopes(packed)
		if err != nil || unpacked.compress().String() != c.Compressed {
			t.Errorf("unpack %q: %v %q, want %q", c.Scopes, err, unpacked, c.Compressed)
		}
	}
	for _, c := range v.Unpack {
		scopes, err := ParseScopes(c.Packed)
		if (err == nil) != (c.Scopes != nil) || (err == nil && scopes.String() != *c.Scopes) {
			t.Errorf("unpack %v: %v %q, want %v", c.Packed, err, scopes, c.Scopes)
		}
	}
	for _, c := range v.Expand.Cases {
		if got := scopesText(t, c.Scopes).Expand(v.Expand.Depends).String(); got != c.Expanded {
			t.Errorf("expand %q: %q, want %q", c.Scopes, got, c.Expanded)
		}
	}
}

// sessionVectors is ../_golden/session.json.
type sessionVectors struct {
	ID       string
	Secret   string
	Sessions []struct {
		Iat, Exp int64
		Scopes   string
		Sub      *string
		Token    string
	}
	User struct {
		Iss, Sub, Scopes, Token string
		Iat, Exp                int64
		Amr                     []Amr
	}
}

// at sets the signing clock to unix seconds until the test ends.
func at(t *testing.T, unix int64) {
	t.Helper()
	before := now
	now = func() time.Time { return time.Unix(unix, 0) }
	t.Cleanup(func() { now = before })
}

func TestSecretSignsTheServersBytes(t *testing.T) {
	var v sessionVectors
	read(t, "session.json", &v)
	secret, err := ParseSecret(v.ID, v.Secret)
	if err != nil {
		t.Fatal(err)
	}
	if len(v.Sessions) == 0 {
		t.Fatal("no sessions")
	}
	for _, c := range v.Sessions {
		at(t, c.Iat)
		opts := []IssueOption{WithScopes(scopesText(t, c.Scopes)), WithTTL(time.Duration(c.Exp-c.Iat) * time.Second)}
		if c.Sub != nil {
			opts = append(opts, WithSubject(*c.Sub))
		}
		session, err := secret.Issue(opts...)
		if err != nil {
			t.Fatal(err)
		}
		if session.Token != c.Token {
			t.Errorf("token for %q:\n got %s\nwant %s", c.Scopes, session.Token, c.Token)
		}
		parsed, err := ParseSession(c.Token)
		if err != nil {
			t.Fatal(err)
		}
		want := scopesText(t, c.Scopes).Expand(Depends).Pack()
		if parsed.Issuer != v.ID || !reflect.DeepEqual(parsed.Scopes.Pack(), want) || parsed.IssuedAt.Unix() != c.Iat ||
			parsed.ExpiresAt.Unix() != c.Exp {
			t.Errorf("parsed %q: %+v", c.Token, parsed)
		}
	}
	if _, err := secret.Issue(WithTTL(MaxTTL + time.Second)); err == nil {
		t.Error("a session over 24 h signed")
	}
	if secret.String() != v.ID {
		t.Error("a secret prints more than its id")
	}
}

func TestParseSecretRefusesATypo(t *testing.T) {
	var v sessionVectors
	read(t, "session.json", &v)
	typo := []byte(v.Secret)
	if typo[10] == 'a' {
		typo[10] = 'b'
	} else {
		typo[10] = 'a'
	}
	for _, c := range []struct{ id, secret string }{
		{v.ID, string(typo)},
		{v.ID, "his/short"},
		{v.ID, "his/0OIl"},
		{v.ID, strings.TrimPrefix(v.Secret, "his/")},
		{"user/x", v.Secret},
		{"hip/", v.Secret},
	} {
		if _, err := ParseSecret(c.id, c.secret); err == nil {
			t.Errorf("%q %q: parsed", c.id, c.secret)
		}
	}
}

func TestParseSessionReadsAServerToken(t *testing.T) {
	var v sessionVectors
	read(t, "session.json", &v)
	session, err := ParseSession(v.User.Token)
	if err != nil {
		t.Fatal(err)
	}
	if session.Issuer != v.User.Iss || session.Subject != v.User.Sub || session.Scopes.String() != v.User.Scopes ||
		session.IssuedAt.Unix() != v.User.Iat || session.ExpiresAt.Unix() != v.User.Exp ||
		!reflect.DeepEqual(session.Amr, v.User.Amr) {
		t.Errorf("%+v", session)
	}
	if session.Expired(time.Unix(v.User.Exp-1, 0)) || !session.Expired(time.Unix(v.User.Exp, 0)) {
		t.Error("expiry is exp")
	}
	if !session.Can(walletGet, map[string]string{"id": "x"}) {
		t.Error("com.hashinbox cannot read a wallet")
	}
	if _, err := ParseSession("a.b"); err == nil {
		t.Error("two parts parsed")
	}
}

func TestFillEscapesInOnePass(t *testing.T) {
	got := Fill("a.w[{id}].x[{other}]:read", map[string]string{"id": "{other}] %", "other": "y"})
	if got != "a.w[{other}%5D%20%25].x[y]:read" {
		t.Error(got)
	}
	if got := Fill("a[{missing}]:read", nil); got != "a[{missing}]:read" {
		t.Error(got)
	}
}
