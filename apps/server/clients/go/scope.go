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

// Scope is one scope: a path of resources, full or read only, as text
// `com.hashinbox.wallets[id]:read`.
type Scope struct {
	path []segment
	read bool
}

// segment is one step of a scope's path: a resource, all of it or the one
// id names.
type segment struct {
	name  string
	id    string
	hasID bool
}

// readSuffix ends a read-only scope.
const readSuffix = ":read"

// ParseScope is the scope text is: steps split by `.` (not inside `[`
// `]`), then `:read` or nothing.
func ParseScope(text string) (Scope, error) {
	var steps []string
	start, inside, end := 0, false, len(text)
loop:
	for at := 0; at < len(text); at++ {
		switch b := text[at]; {
		case b == '[' && !inside:
			inside = true
		case b == ']' && inside:
			inside = false
		case b == '.' && !inside:
			steps = append(steps, text[start:at])
			start = at + 1
		case b == ':' && !inside:
			end = at
			break loop
		}
	}
	if inside {
		return Scope{}, fmt.Errorf("scope: bad id in %q", text)
	}
	if rest := text[end:]; rest != "" && rest != readSuffix {
		return Scope{}, fmt.Errorf("scope: %q does not end with %s or nothing", text, readSuffix)
	}
	if text == "" {
		return Scope{}, errors.New("scope: a scope needs a resource")
	}
	steps = append(steps, text[start:end])
	path := make([]segment, 0, len(steps))
	for _, step := range steps {
		parsed, err := parseSegment(step)
		if err != nil {
			return Scope{}, err
		}
		path = append(path, parsed)
	}
	return Scope{path: path, read: end < len(text)}, nil
}

// parseSegment is the step text is: `name` or `name[id]`.
func parseSegment(text string) (segment, error) {
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
	text := joinSteps(s.path)
	if s.read {
		text += readSuffix
	}
	return text
}

func (s segment) String() string {
	if !s.hasID {
		return s.name
	}
	return s.name + "[" + EscapeID(s.id) + "]"
}

// Covers reports whether s allows everything other allows: s is full or
// other read only, and s's path is a prefix of other's, step by step (a
// name covers the same name and its ids). A scope so covers its whole
// subtree; a child never its parent.
func (s Scope) Covers(other Scope) bool {
	return (!s.read || other.read) && s.reaches(other.path)
}

// reaches reports whether its path is a prefix of path, step by step.
func (s Scope) reaches(path []segment) bool {
	if len(s.path) > len(path) {
		return false
	}
	for at, step := range s.path {
		if step.name != path[at].name || step.hasID && (!path[at].hasID || step.id != path[at].id) {
			return false
		}
	}
	return true
}

// compare orders scopes as packages/scope does: by path step by step (a
// prefix first), then full before read only.
func (s Scope) compare(other Scope) int {
	for at := 0; at < len(s.path) && at < len(other.path); at++ {
		if c := s.path[at].compare(other.path[at]); c != 0 {
			return c
		}
	}
	if c := len(s.path) - len(other.path); c != 0 {
		return c
	}
	switch {
	case s.read == other.read:
		return 0
	case s.read:
		return 1
	}
	return -1
}

// compare orders steps by name, then all of a resource before one id.
func (s segment) compare(other segment) int {
	if c := strings.Compare(s.name, other.name); c != 0 {
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

// Scopes are what a session holds.
type Scopes []Scope

// ParseScopes is packed unpacked: each prefix and its scopes joined by `.`,
// `*` the prefix itself (an empty prefix: the scopes as they are).
func ParseScopes(packed map[string]ScopeGroup) (Scopes, error) {
	prefixes := make([]string, 0, len(packed))
	for prefix := range packed {
		prefixes = append(prefixes, prefix)
	}
	sort.Strings(prefixes)
	var scopes Scopes
	for _, prefix := range prefixes {
		for _, text := range packed[prefix].texts() {
			scope, err := ParseScope(joined(prefix, text))
			if err != nil {
				return nil, err
			}
			scopes = append(scopes, scope)
		}
	}
	return scopes, nil
}

// joined is text under prefix: `*` (`*:read`) the prefix itself.
func joined(prefix, text string) string {
	level, star := strings.CutPrefix(text, "*")
	switch {
	case prefix == "":
		return text
	case star && (level == "" || strings.HasPrefix(level, ":")):
		return prefix + level
	}
	return prefix + "." + text
}

// texts is the group's scopes, one or several.
func (g ScopeGroup) texts() []string {
	if g.String != nil {
		return []string{*g.String}
	}
	if g.List != nil {
		return *g.List
	}
	return nil
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

// Expand is them and every scope depends adds, to a fixed point,
// compressed. depends maps a source path (`com.hashinbox.triggers`) to the
// scopes holding it needs. A held scope triggers a source when it covers
// the source or an id of it, its own ids ignored; each target is added
// whole, read only when written `:read` or the held scope is read only.
// An entry whose text does not parse adds nothing.
func (s Scopes) Expand(depends map[string][]string) Scopes {
	sources := make([]string, 0, len(depends))
	for source := range depends {
		sources = append(sources, source)
	}
	sort.Strings(sources)
	type rule struct {
		source  Scope
		targets Scopes
	}
	var rules []rule
	for _, text := range sources {
		source, err := ParseScope(text)
		if err != nil {
			continue
		}
		targets := Scopes{}
		for _, target := range depends[text] {
			parsed, err := ParseScope(target)
			if err != nil {
				targets = nil
				break
			}
			targets = append(targets, parsed)
		}
		if targets != nil {
			rules = append(rules, rule{source, targets})
		}
	}
	held := append(Scopes{}, s...)
	for at := 0; at < len(held); at++ {
		scope := held[at]
		for _, r := range rules {
			if !triggers(scope, r.source) {
				continue
			}
			for _, target := range r.targets {
				target.read = target.read || scope.read
				if !held.Covers(target) {
					held = append(held, target)
				}
			}
		}
	}
	return held.compress()
}

// triggers reports whether held triggers source's dependencies: it covers
// source's path, its ids ignored.
func triggers(held, source Scope) bool {
	path := make([]segment, len(held.path))
	for at, step := range held.path {
		path[at] = segment{name: step.name}
	}
	return Scope{path: path}.reaches(source.path)
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
// `{"com.hashinbox": ["triggers", "wallets[a]:read"]}`. A prefix is the
// leading plain names every scope starting with the same step shares; a
// scope that is its prefix is `*` (`*:read`). A group of one scope is its
// text alone: `{"com.hashinbox": "*"}`. ParseScopes reverses it.
func (s Scopes) Pack() map[string]ScopeGroup {
	compressed := s.compress()
	lists := map[string][]string{}
	for start := 0; start < len(compressed); {
		end := start + 1
		for end < len(compressed) && compressed[end].path[0] == compressed[start].path[0] {
			end++
		}
		group := compressed[start:end]
		shared := sharedPrefix(group)
		for _, scope := range group {
			prefix := joinSteps(scope.path[:shared])
			rest := Scope{path: scope.path[shared:], read: scope.read}.String()
			if len(scope.path) == shared {
				rest = "*" + rest
			}
			lists[prefix] = append(lists[prefix], rest)
		}
		start = end
	}
	packed := map[string]ScopeGroup{}
	for prefix, list := range lists {
		if len(list) == 1 {
			packed[prefix] = ScopeGroup{String: &list[0]}
		} else {
			packed[prefix] = ScopeGroup{List: &list}
		}
	}
	return packed
}

// sharedPrefix is how many leading steps of scopes are the same plain name
// in each.
func sharedPrefix(scopes Scopes) int {
	shortest := len(scopes[0].path)
	for _, scope := range scopes {
		shortest = min(shortest, len(scope.path))
	}
	first := scopes[0].path
	at := 0
	for ; at < shortest; at++ {
		if first[at].hasID {
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
