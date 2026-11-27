package hashinbox

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"testing"
	"time"
)

// seen is what a test server was sent.
type seen struct {
	mu       sync.Mutex
	requests []string // "METHOD escaped-path?query body"
	bearers  []string
}

func (s *seen) add(r *http.Request) {
	body, _ := io.ReadAll(r.Body)
	s.mu.Lock()
	defer s.mu.Unlock()
	line := r.Method + " " + r.URL.EscapedPath()
	if r.URL.RawQuery != "" {
		line += "?" + r.URL.RawQuery
	}
	if len(body) > 0 {
		line += " " + string(body)
	}
	s.requests = append(s.requests, line)
	s.bearers = append(s.bearers, strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer "))
}

// serve is a test server answering with handle, recording each request.
func serve(t *testing.T, handle func(w http.ResponseWriter, r *http.Request, n int)) (*httptest.Server, *seen) {
	t.Helper()
	s := &seen{}
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		s.add(r)
		s.mu.Lock()
		n := len(s.requests)
		s.mu.Unlock()
		handle(w, r, n)
	}))
	t.Cleanup(server.Close)
	return server, s
}

func answer(w http.ResponseWriter, status int, body string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	io.WriteString(w, body)
}

func TestDoFillsThePathQueryBodyAndBearer(t *testing.T) {
	server, seen := serve(t, func(w http.ResponseWriter, r *http.Request, n int) {
		switch r.URL.Path {
		case "/openapi.json":
			answer(w, 200, `{"openapi":"3.1.0"}`)
		case "/v1/wallets":
			answer(w, 200, `{"wallets":[],"next_cursor":null}`)
		default:
			answer(w, 201, `{"asset":"native"}`)
		}
	})
	session := Session{Token: "jwt", Scopes: MustScopes("com.hashinbox.wallets[a%20b/c]", "com.hashinbox.wallets:read")}
	client := NewClient(server.URL+"/", WithSession(session))
	ctx := context.Background()

	start := int64(7)
	index, err := WalletIndexesCreate(ctx, client, WalletIndexesCreateRequest{
		ID:                       "a b/c",
		CreateWalletIndexRequest: CreateWalletIndexRequest{Asset: "native", Start: &start},
	})
	if err != nil || index.Asset != "native" {
		t.Fatal(index, err)
	}
	limit, cursor := int64(5), "x&y"
	if _, err := WalletsList(ctx, client, WalletsListRequest{Limit: &limit, Cursor: &cursor}); err != nil {
		t.Fatal(err)
	}
	document, err := OpenapiGet(ctx, client, OpenapiGetRequest{})
	if err != nil || string(document.Value) != `{"openapi":"3.1.0"}` {
		t.Fatal(string(document.Value), err)
	}
	want := []string{
		`POST /v1/wallets/a%20b%2Fc/indexes {"asset":"native","start":7}`,
		`GET /v1/wallets?cursor=x%26y&limit=5`,
		`GET /openapi.json`,
	}
	if fmt.Sprint(seen.requests) != fmt.Sprint(want) {
		t.Errorf("sent %q\nwant %q", seen.requests, want)
	}
	for _, bearer := range seen.bearers {
		if bearer != "jwt" {
			t.Errorf("bearer %q", bearer)
		}
	}
}

func TestDoMapsTheErrorBody(t *testing.T) {
	server, _ := serve(t, func(w http.ResponseWriter, r *http.Request, n int) {
		if n == 1 {
			answer(w, 403, `{"kind":"permission_denied","message":"no","reason":"missing_scope","metadata":{"scope":"com.hashinbox.wallets:read"}}`)
			return
		}
		answer(w, 503, `{"status":"down"}`)
	})
	client := NewClient(server.URL, WithoutScopeCheck())
	_, err := WalletsList(context.Background(), client, WalletsListRequest{})
	var api *APIError
	if !errors.As(err, &api) || api.Status != 403 || api.Kind != KindPermissionDenied || api.Message != "no" ||
		api.Metadata["scope"] != "com.hashinbox.wallets:read" || APIErrorFrom(err).Reason != ReasonMissingScope {
		t.Fatalf("%#v", err)
	}
	_, err = WalletsList(context.Background(), client, WalletsListRequest{})
	if !errors.As(err, &api) || api.Status != 503 || api.Kind != KindUnavailable || api.Metadata["response"] != `{"status":"down"}` {
		t.Fatalf("%#v", err)
	}
}

func TestDoRefusesAMissingScopeLocally(t *testing.T) {
	server, seen := serve(t, func(w http.ResponseWriter, r *http.Request, n int) {
		answer(w, 200, `{}`)
	})
	session := Session{Token: "jwt", Scopes: MustScopes("com.hashinbox.wallets[x]:read")}
	client := NewClient(server.URL, WithSession(session))
	_, err := WalletGet(context.Background(), client, WalletGetRequest{ID: "y"})
	var api *APIError
	if !errors.As(err, &api) || api.Status != 403 || api.Kind != KindPermissionDenied ||
		api.Reason != ReasonMissingScope || api.Message != "wallet_get needs scope com.hashinbox.wallets[y]:read" ||
		api.Metadata["scope"] != "com.hashinbox.wallets[y]:read" || api.Metadata["operation"] != "wallet_get" ||
		APIErrorFrom(err).Reason != ReasonMissingScope {
		t.Fatalf("%#v", err)
	}
	if _, err := WalletGet(context.Background(), client, WalletGetRequest{ID: "x"}); err != nil {
		t.Fatal(err)
	}
	if len(seen.requests) != 1 {
		t.Errorf("sent %q", seen.requests)
	}
}

// testSecret is the vectors' key.
func testSecret(t *testing.T) Secret {
	t.Helper()
	var v sessionVectors
	read(t, "session.json", &v)
	secret, err := ParseSecret(v.ID, v.Secret)
	if err != nil {
		t.Fatal(err)
	}
	return secret
}

// keyServer answers GET /v1/session with the key's scopes, and each other
// call with call's answer.
func keyServer(t *testing.T, call func(w http.ResponseWriter, n int)) (*httptest.Server, *seen) {
	calls := 0
	return serve(t, func(w http.ResponseWriter, r *http.Request, n int) {
		if r.URL.Path == "/v1/session" {
			answer(w, 200, `{"kind":"key","key":{"id":"hip/x","scopes":{"com.hashinbox":["wallets"]}},"scopes":{}}`)
			return
		}
		calls++
		call(w, calls)
	})
}

func TestWithSecretSignsItsOwnSessions(t *testing.T) {
	secret := testSecret(t)
	at(t, 1_700_000_000)
	server, seen := keyServer(t, func(w http.ResponseWriter, n int) {
		answer(w, 200, `{"wallets":[],"next_cursor":null}`)
	})
	client := NewClient(server.URL, WithSecret(secret))
	ctx := context.Background()
	for range 2 {
		if _, err := WalletsList(ctx, client, WalletsListRequest{}); err != nil {
			t.Fatal(err)
		}
	}
	at(t, 1_700_000_000+int64((defaultTTL-renewBefore)/time.Second))
	if _, err := WalletsList(ctx, client, WalletsListRequest{}); err != nil {
		t.Fatal(err)
	}
	want := "[GET /v1/session GET /v1/wallets GET /v1/wallets GET /v1/wallets]"
	if fmt.Sprint(seen.requests) != want {
		t.Errorf("sent %q", seen.requests)
	}
	probe, err := ParseSession(seen.bearers[0])
	if err != nil || len(probe.Scopes) != 0 {
		t.Fatal("the probe holds scopes", probe, err)
	}
	first, err := ParseSession(seen.bearers[1])
	if err != nil || first.Scopes.String() != "com.hashinbox.jobs com.hashinbox.wallets" || first.ExpiresAt.Sub(first.IssuedAt) != defaultTTL {
		t.Fatal(first, err)
	}
	if seen.bearers[2] != seen.bearers[1] || seen.bearers[3] == seen.bearers[1] {
		t.Error("not cached, or not renewed near expiry")
	}
	for _, bearer := range seen.bearers {
		if strings.Contains(bearer, "his/") || strings.Count(bearer, ".") != 2 {
			t.Errorf("bearer %q is not a JWT", bearer)
		}
	}
}

func TestWithSecretRenewsOnceAfterSessionExpired(t *testing.T) {
	secret := testSecret(t)
	expired := `{"kind":"unauthenticated","message":"expired","reason":"session_expired"}`
	server, seen := keyServer(t, func(w http.ResponseWriter, n int) {
		if n == 1 || n >= 3 {
			answer(w, 401, expired)
			return
		}
		answer(w, 200, `{"wallets":[],"next_cursor":null}`)
	})
	client := NewClient(server.URL, WithSecret(secret), WithSecretScopes(MustScopes("com.hashinbox.wallets:read")))
	ctx := context.Background()
	if _, err := WalletsList(ctx, client, WalletsListRequest{}); err != nil {
		t.Fatal("not retried:", err)
	}
	if _, err := WalletsList(ctx, client, WalletsListRequest{}); APIErrorFrom(err).Reason != ReasonSessionExpired {
		t.Fatal("retried more than once, or hid the error:", err)
	}
	want := "[GET /v1/wallets GET /v1/wallets GET /v1/wallets GET /v1/wallets]"
	if fmt.Sprint(seen.requests) != want {
		t.Errorf("sent %q", seen.requests)
	}
}

func TestLoginSurfacesItsReasonsAndKeepsTheSession(t *testing.T) {
	var v sessionVectors
	read(t, "session.json", &v)
	server, seen := serve(t, func(w http.ResponseWriter, r *http.Request, n int) {
		switch n {
		case 1:
			answer(w, 401, `{"kind":"unauthenticated","message":"code","reason":"totp_required"}`)
		case 2:
			answer(w, 401, `{"kind":"unauthenticated","message":"wrong","reason":"wrong_credentials"}`)
		case 3:
			body, _ := json.Marshal(map[string]any{"token": v.User.Token, "expires_at": v.User.Exp, "scopes": map[string]any{}})
			answer(w, 201, string(body))
		default:
			w.WriteHeader(204)
		}
	})
	client := NewClient(server.URL)
	ctx := context.Background()
	login := LoginRequest{Email: "a@b.c", Password: "pw"}
	if _, err := client.Login(ctx, login); APIErrorFrom(err).Reason != ReasonTOTPRequired {
		t.Fatal(err)
	}
	login.TOTP = "123456"
	if _, err := client.Login(ctx, login); APIErrorFrom(err).Reason != ReasonWrongCredentials {
		t.Fatal(err)
	}
	session, err := client.Login(ctx, login)
	if err != nil || session.Issuer != v.User.Iss {
		t.Fatal(session, err)
	}
	if current, ok := client.Session(); !ok || current.Token != v.User.Token {
		t.Fatal("not kept")
	}
	if err := client.Logout(ctx, true); err != nil {
		t.Fatal(err)
	}
	if _, ok := client.Session(); ok {
		t.Fatal("kept after logout")
	}
	if seen.bearers[2] != "" || seen.bearers[3] != v.User.Token {
		t.Errorf("bearers %q", seen.bearers)
	}
	if !strings.HasPrefix(seen.requests[2], `POST /v1/session {"email":"a@b.c","password":"pw","totp":"123456"}`) ||
		seen.requests[3] != "DELETE /v1/session?all=true" {
		t.Errorf("sent %q", seen.requests)
	}
}

func TestStreamReadsEventsUntilCancelled(t *testing.T) {
	server, _ := serve(t, func(w http.ResponseWriter, r *http.Request, n int) {
		w.Header().Set("Content-Type", "text/event-stream")
		io.WriteString(w, ": hello\n\nevent: missed\ndata: {\"missed\":2}\n\ndata: a\ndata: b\nid: 7\n\n")
		w.(http.Flusher).Flush()
		select {
		case <-r.Context().Done():
		case <-time.After(10 * time.Second):
		}
	})
	session := Session{Token: "jwt", Scopes: MustScopes("com.hashinbox.logs:read")}
	client := NewClient(server.URL, WithSession(session))
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	stream, err := LogsStream(ctx, client, LogsStreamRequest{})
	if err != nil {
		t.Fatal(err)
	}
	defer stream.Close()
	first, err := stream.Next()
	if err != nil || first != (RawEvent{Event: "missed", Data: `{"missed":2}`}) {
		t.Fatal(first, err)
	}
	second, err := stream.Next()
	if err != nil || second != (RawEvent{Event: "message", Data: "a\nb", ID: "7"}) {
		t.Fatal(second, err)
	}
	cancel()
	if _, err := stream.Next(); err == nil {
		t.Fatal("not cancelled")
	}
}

// windows is a stream server: answer n (1-based) as answers[n-1]
// (`503`/`401`: that status; else that body, then its end), recording each
// `cursor` query; past the last, open until the client leaves.
func windows(t *testing.T, answers ...string) (*HTTPClient, *[]string, *sync.Mutex) {
	var mu sync.Mutex
	var ids []string
	server, _ := serve(t, func(w http.ResponseWriter, r *http.Request, n int) {
		mu.Lock()
		ids = append(ids, r.URL.Query().Get("cursor"))
		mu.Unlock()
		if n > len(answers) {
			w.Header().Set("Content-Type", "text/event-stream")
			w.(http.Flusher).Flush()
			<-r.Context().Done()
			return
		}
		switch body := answers[n-1]; body {
		case "503", "401":
			status := map[string]int{"503": 503, "401": 401}[body]
			answer(w, status, `{"kind":"unavailable","message":"no","metadata":{}}`)
		default:
			w.Header().Set("Content-Type", "text/event-stream")
			io.WriteString(w, body)
		}
	})
	session := Session{Token: "jwt", Scopes: MustScopes("com.hashinbox.logs:read")}
	client := NewClient(server.URL, WithSession(session), WithStreamBackoff(time.Millisecond, 4*time.Millisecond))
	return client, &ids, &mu
}

// readData is the data of the next n events of stream.
func readData(t *testing.T, stream *Stream[RawEvent], n int) []string {
	t.Helper()
	var data []string
	for range n {
		event, err := stream.Next()
		if err != nil {
			t.Fatal(data, err)
		}
		data = append(data, event.Data)
	}
	return data
}

func TestStreamReconnectsAfterEachWindowFromTheLastID(t *testing.T) {
	client, ids, mu := windows(t,
		"data: a\nid: 1\n\ndata: b\nid: 2\n\nevent: close\ndata: {}\n\n",
		"data: c\nid: 3\n\nevent: close\ndata: {}\n\n",
		"data: d\nid: 4\n\n",
	)
	own := "0"
	stream, err := LogsStream(context.Background(), client, LogsStreamRequest{Cursor: &own})
	if err != nil {
		t.Fatal(err)
	}
	defer stream.Stop()
	if got := strings.Join(readData(t, stream, 4), ""); got != "abcd" {
		t.Fatal(got)
	}
	if _, err := stream.Next(); err != io.EOF {
		t.Fatal("an answer without close ends it:", err)
	}
	mu.Lock()
	defer mu.Unlock()
	if strings.Join(*ids, ",") != "0,2,3" || stream.Cursor() != "4" {
		t.Fatal(*ids, stream.Cursor())
	}
}

func TestStreamStopsReconnecting(t *testing.T) {
	client, ids, mu := windows(t, "data: a\nid: 1\n\nevent: close\ndata: {}\n\n")
	stream, err := LogsStream(context.Background(), client, LogsStreamRequest{})
	if err != nil {
		t.Fatal(err)
	}
	readData(t, stream, 1)
	go func() {
		time.Sleep(50 * time.Millisecond)
		stream.Stop()
	}()
	if _, err := stream.Next(); err != io.EOF {
		t.Fatal(err)
	}
	time.Sleep(20 * time.Millisecond)
	mu.Lock()
	defer mu.Unlock()
	if len(*ids) != 2 {
		t.Fatal("reconnected after Stop:", *ids)
	}
}

func TestStreamBacksOffOnErrorsAndEndsOnARefusal(t *testing.T) {
	client, ids, mu := windows(t,
		"data: a\nid: 1\n\nevent: close\ndata: {}\n\n",
		"503", "503",
		"data: b\nid: 2\n\nevent: close\ndata: {}\n\n",
		"401",
	)
	stream, err := LogsStream(context.Background(), client, LogsStreamRequest{})
	if err != nil {
		t.Fatal(err)
	}
	started := time.Now()
	if got := strings.Join(readData(t, stream, 2), ""); got != "ab" {
		t.Fatal(got)
	}
	if waited := time.Since(started); waited < 3*time.Millisecond {
		t.Fatal("no backoff:", waited)
	}
	_, err = stream.Next()
	var refused *APIError
	if !errors.As(err, &refused) || refused.Status != 401 {
		t.Fatal(err)
	}
	if _, err := stream.Next(); err != io.EOF {
		t.Fatal(err)
	}
	mu.Lock()
	defer mu.Unlock()
	if strings.Join(*ids, ",") != ",1,1,1,2" {
		t.Fatal(*ids)
	}
}

func TestAPIErrorFrom(t *testing.T) {
	api := &APIError{Status: 401, Kind: KindUnauthenticated, Reason: ReasonTOTPRequired}
	if APIErrorFrom(api) != api || APIErrorFrom(fmt.Errorf("login: %w", api)) != api {
		t.Fatal("not as is")
	}
	if APIErrorFrom(api).Reason != ReasonTOTPRequired {
		t.Fatal(api)
	}
	transport := &url.Error{Op: "Get", URL: "http://x", Err: errors.New("refused")}
	if got := APIErrorFrom(transport); got.Status != 0 || got.Kind != KindUnavailable || got.Reason != "" ||
		got.Message != transport.Error() || !errors.Is(got, transport) {
		t.Fatalf("%#v", got)
	}
	if got := APIErrorFrom(context.DeadlineExceeded); got.Kind != KindUnavailable {
		t.Fatalf("%#v", got)
	}
	plain := errors.New("boom")
	if got := APIErrorFrom(plain); got.Kind != KindUnavailable || got.Message != "boom" || errors.Unwrap(got) != plain {
		t.Fatalf("%#v", got)
	}
	if got := APIErrorFrom("odd"); got.Status != 0 || got.Kind != KindUnavailable || got.Message != "odd" || got.Cause != nil {
		t.Fatalf("%#v", got)
	}
	if got := APIErrorFrom(nil); got == nil || got.Kind != KindUnavailable || got.Cause != nil {
		t.Fatalf("%#v", got)
	}
}

func TestAPIErrorFromAnAnswerReadsOnlyAnErrorBody(t *testing.T) {
	unparsed := func(status int, raw, response string) {
		t.Helper()
		got := APIErrorFrom(httpAnswer{status, []byte(raw)})
		want := fmt.Sprintf("could not parse the response (HTTP %d)", status)
		if got.Status != status || got.Kind != KindUnavailable || got.Reason != "" || got.Message != want ||
			len(got.Metadata) != 1 || got.Metadata["response"] != response {
			t.Fatalf("%s: %#v", raw, got)
		}
	}
	html := "<html>" + strings.Repeat("x", 2000) + "</html>"
	unparsed(502, html, html[:1024])
	unparsed(503, "", "")
	for _, raw := range []string{`{"status":"down"}`, `{"kind":"nope","message":"x"}`, `{"kind":"internal"}`} {
		unparsed(503, raw, raw)
	}
	raw := `{"kind":"unauthenticated","message":"code","reason":"totp_required","metadata":{"a":"b"}}`
	got := APIErrorFrom(httpAnswer{401, []byte(raw)})
	if got.Status != 401 || got.Kind != KindUnauthenticated || got.Message != "code" || got.Reason != ReasonTOTPRequired ||
		got.Metadata["a"] != "b" {
		t.Fatalf("%#v", got)
	}
}
