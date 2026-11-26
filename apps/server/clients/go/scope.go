// Hand-written: packages/scope ported (grammar, coverage, escaping, pack),
// kept honest by ../_golden/scope.json.

package hashinbox

import (
	"errors"
	"fmt"
	"sort"
	"strings"
	"unicode/utf8"
)

// Scope is one scope: a path of resources and an action, as text
// `com.hashinbox.wallets[id].$read`.
type Scope struct {
	path   []segment
	action string // a name, or "*": any
}

// segment is one step of a scope's path: a resource, all of it or the one
// id names, or "*": any one step (at the end, anything below).
type segment struct {
	name  string // a name, or "*"
	id    string
	hasID bool
}

// wildcard is the step or action `*`.
const wildcard = "*"

// ParseScope is the scope text is: steps split by `.` (not inside `[`
// `]`), the last one the action, `$name` or `$*`.
func ParseScope(text string) (Scope, error) {
	var steps []string
	start, inside := 0, false
	for at := 0; at < len(text); at++ {
		switch b := text[at]; {
		case b == '[' && !inside:
			inside = true
		case b == ']' && inside:
			inside = false
		case b == '.' && !inside:
			steps = append(steps, text[start:at])
			start = at + 1
		}
	}
	if inside {
		return Scope{}, fmt.Errorf("scope: bad id in %q", text)
	}
	action, ok := strings.CutPrefix(text[start:], "$")
	if !ok || (action != wildcard && !isName(action)) {
		return Scope{}, fmt.Errorf("scope: %q does not end with $action", text)
	}
	path := make([]segment, 0, len(steps))
	for _, step := range steps {
		parsed, err := parseSegment(step)
		if err != nil {
			return Scope{}, err
		}
		path = append(path, parsed)
	}
	if len(path) == 0 {
		return Scope{}, errors.New("scope: a scope needs a resource")
	}
	return Scope{path: path, action: action}, nil
}

// parseSegment is the step text is: `*`, `name` or `name[id]`.
func parseSegment(text string) (segment, error) {
	if text == wildcard {
		return segment{name: wildcard}, nil
	}
	name, rest, found := strings.Cut(text, "[")
	if !found {
		if !isName(text) {
			return segment{}, fmt.Errorf("scope: %q is not a name", text)
		}
		return segment{name: text}, nil
	}
	escaped, ok := strings.CutSuffix(rest, "]")
	if !ok {
		return segment{}, fmt.Errorf("scope: bad id in %q", text)
	}
	id, err := unescape(escaped)
	if err != nil {
		return segment{}, err
	}
	if id == "" {
		return segment{}, fmt.Errorf("scope: bad id in %q", text)
	}
	if !isName(name) {
		return segment{}, fmt.Errorf("scope: %q is not a name", name)
	}
	return segment{name: name, id: id, hasID: true}, nil
}

// isName reports whether text is ASCII letters, digits, `_` and `-`, one
// at least.
func isName(text string) bool {
	if text == "" {
		return false
	}
	for i := 0; i < len(text); i++ {
		b := text[i]
		if !('a' <= b && b <= 'z' || 'A' <= b && b <= 'Z' || '0' <= b && b <= '9' || b == '_' || b == '-') {
			return false
		}
	}
	return true
}

// EscapeID is id as written between a scope's brackets: `%`, `[`, `]`,
// ASCII whitespace and controls as %XX.
func EscapeID(id string) string {
	var out strings.Builder
	for _, r := range id {
		if r == '%' || r == '[' || r == ']' || r <= ' ' || r == 0x7f {
			fmt.Fprintf(&out, "%%%02X", r)
		} else {
			out.WriteRune(r)
		}
	}
	return out.String()
}

// unescape is the id text (as written between `[` and `]`) is.
func unescape(text string) (string, error) {
	out := make([]byte, 0, len(text))
	for at := 0; at < len(text); {
		if text[at] != '%' {
			out = append(out, text[at])
			at++
			continue
		}
		if at+3 > len(text) {
			return "", fmt.Errorf("scope: bad escape in %q", text)
		}
		b, ok := hexByte(text[at+1], text[at+2])
		if !ok {
			return "", fmt.Errorf("scope: bad escape in %q", text)
		}
		out = append(out, b)
		at += 3
	}
	if !utf8.Valid(out) {
		return "", fmt.Errorf("scope: bad escape in %q", text)
	}
	return string(out), nil
}

// hexByte is the byte two hex digits are. As Rust's u8::from_str_radix, a
// leading `+` is a sign: `%+5` is 0x05.
func hexByte(hi, lo byte) (byte, bool) {
	l, ok := hexDigit(lo)
	if hi == '+' || !ok {
		return l, ok
	}
	h, ok := hexDigit(hi)
	return h<<4 | l, ok
}

func hexDigit(b byte) (byte, bool) {
	switch {
	case '0' <= b && b <= '9':
		return b - '0', true
	case 'a' <= b && b <= 'f':
		return b - 'a' + 10, true
	case 'A' <= b && b <= 'F':
		return b - 'A' + 10, true
	}
	return 0, false
}

// String is its text; it parses back to it.
func (s Scope) String() string {
	var out strings.Builder
	for _, step := range s.path {
		out.WriteString(step.String())
		out.WriteByte('.')
	}
	out.WriteString("$" + s.action)
	return out.String()
}

func (s segment) String() string {
	if !s.hasID {
		return s.name
	}
	return s.name + "[" + EscapeID(s.id) + "]"
}

// Covers reports whether s allows everything other allows: its action the
// same or `$*`, its path matching step by step (a name covers the same
// name and its ids; `*` in the middle any one step; `*` at the end
// anything below, nothing included).
func (s Scope) Covers(other Scope) bool {
	actions := s.action == wildcard || s.action == other.action
	return actions && pathCovers(s.path, other.path)
}

func pathCovers(ours, theirs []segment) bool {
	if len(ours) == 0 {
		return len(theirs) == 0
	}
	if len(ours) == 1 && ours[0].name == wildcard {
		return true
	}
	if len(theirs) == 0 {
		return false
	}
	return ours[0].covers(theirs[0]) && pathCovers(ours[1:], theirs[1:])
}

func (s segment) covers(other segment) bool {
	if s.name == wildcard {
		return true
	}
	if other.name == wildcard || s.name != other.name {
		return false
	}
	return !s.hasID || (other.hasID && s.id == other.id)
}

// compare orders scopes as packages/scope does: by path, then action; a
// name before `*`, all of a resource before one id.
func (s Scope) compare(other Scope) int {
	for at := 0; at < len(s.path) && at < len(other.path); at++ {
		if c := s.path[at].compare(other.path[at]); c != 0 {
			return c
		}
	}
	if c := len(s.path) - len(other.path); c != 0 {
		return c
	}
	return compareNames(s.action, other.action)
}

func (s segment) compare(other segment) int {
	if c := compareNames(s.name, other.name); c != 0 || s.name == wildcard {
		return c
	}
	switch {
	case s.hasID != other.hasID && s.hasID:
		return 1
	case s.hasID != other.hasID:
		return -1
	}
	return strings.Compare(s.id, other.id)
}

// compareNames orders two names, `*` after every name.
func compareNames(a, b string) int {
	switch {
	case a == b:
		return 0
	case a == wildcard:
		return 1
	case b == wildcard:
		return -1
	}
	return strings.Compare(a, b)
}

// Scopes are what a session holds.
type Scopes []Scope

// ParseScopes is packed unpacked: each prefix and its scopes joined by `.`
// (an empty prefix: the scopes as they are).
func ParseScopes(packed map[string][]string) (Scopes, error) {
	prefixes := make([]string, 0, len(packed))
	for prefix := range packed {
		prefixes = append(prefixes, prefix)
	}
	sort.Strings(prefixes)
	var scopes Scopes
	for _, prefix := range prefixes {
		for _, text := range packed[prefix] {
			if prefix != "" {
				text = prefix + "." + text
			}
			scope, err := ParseScope(text)
			if err != nil {
				return nil, err
			}
			scopes = append(scopes, scope)
		}
	}
	return scopes, nil
}

// MustScopes is texts parsed; it panics on one that does not parse.
func MustScopes(texts ...string) Scopes {
	scopes := make(Scopes, len(texts))
	for i, text := range texts {
		scope, err := ParseScope(text)
		if err != nil {
			panic(err)
		}
		scopes[i] = scope
	}
	return scopes
}

// Covers reports whether one of them covers scope.
func (s Scopes) Covers(scope Scope) bool {
	for _, held := range s {
		if held.Covers(scope) {
			return true
		}
	}
	return false
}

// FirstUncovered is the first of others none of them covers; false when
// they cover every one.
func (s Scopes) FirstUncovered(others Scopes) (Scope, bool) {
	for _, scope := range others {
		if !s.Covers(scope) {
			return scope, true
		}
	}
	return Scope{}, false
}

// String is their text, space-separated.
func (s Scopes) String() string {
	texts := make([]string, len(s))
	for i, scope := range s {
		texts[i] = scope.String()
	}
	return strings.Join(texts, " ")
}

// compress is the fewest scopes allowing the same, sorted: one covered by
// another dropped (of two equal ones, the first kept).
func (s Scopes) compress() Scopes {
	kept := Scopes{}
	for at, scope := range s {
		dropped := false
		for other, held := range s {
			if other != at && held.Covers(scope) && (!scope.Covers(held) || other < at) {
				dropped = true
				break
			}
		}
		if !dropped {
			kept = append(kept, scope)
		}
	}
	sort.SliceStable(kept, func(i, j int) bool { return kept[i].compare(kept[j]) < 0 })
	return kept
}

// Pack is them compressed, then grouped by their shared prefix, the
// shortest text for a JWT claim:
// `{"com.hashinbox": ["markets.$read", "wallets[a].$read"]}`. A prefix is
// the leading plain names every scope starting with the same step shares,
// each scope keeping one step at least. ParseScopes reverses it.
func (s Scopes) Pack() map[string][]string {
	compressed := s.compress()
	packed := map[string][]string{}
	for start := 0; start < len(compressed); {
		end := start + 1
		for end < len(compressed) && compressed[end].path[0] == compressed[start].path[0] {
			end++
		}
		group := compressed[start:end]
		shared := sharedPrefix(group)
		for _, scope := range group {
			prefix := joinSteps(scope.path[:shared])
			rest := Scope{path: scope.path[shared:], action: scope.action}
			packed[prefix] = append(packed[prefix], rest.String())
		}
		start = end
	}
	return packed
}

// sharedPrefix is how many leading steps of scopes are the same plain name
// in each, each scope keeping one step at least.
func sharedPrefix(scopes Scopes) int {
	shortest := len(scopes[0].path)
	for _, scope := range scopes {
		shortest = min(shortest, len(scope.path))
	}
	first := scopes[0].path
	at := 0
	for ; at < shortest-1; at++ {
		if first[at].hasID || first[at].name == wildcard {
			break
		}
		same := true
		for _, scope := range scopes {
			same = same && scope.path[at] == first[at]
		}
		if !same {
			break
		}
	}
	return at
}

// joinSteps is steps as text, split by `.`.
func joinSteps(steps []segment) string {
	texts := make([]string, len(steps))
	for i, step := range steps {
		texts[i] = step.String()
	}
	return strings.Join(texts, ".")
}

// Fill is template with each `{name}` its parameter, escaped as a scope id;
// a hole with no parameter stays.
func Fill(template string, params map[string]string) string {
	return fill(template, params, EscapeID)
}

// fill is template with each `{name}` its parameter, escaped, in one pass.
func fill(template string, params map[string]string, escape func(string) string) string {
	var out strings.Builder
	for {
		open := strings.IndexByte(template, '{')
		if open < 0 {
			break
		}
		end := strings.IndexByte(template[open:], '}')
		if end < 0 {
			break
		}
		name := template[open+1 : open+end]
		value, ok := params[name]
		out.WriteString(template[:open])
		if ok {
			out.WriteString(escape(value))
		} else {
			out.WriteString(template[open : open+end+1])
		}
		template = template[open+end+1:]
	}
	out.WriteString(template)
	return out.String()
}
