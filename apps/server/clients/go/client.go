// Hand-written: what the generated operations (operations.go) and models
// (models.go) use, and HTTPClient, the default Client over net/http. cligen
// (via apps/server/build.rs) writes those two files beside this; it never
// touches this file.

package hashinbox

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"reflect"
	"strings"
	"sync"
	"time"
)

// Operation is an operation's static description.
type Operation struct {
	ID        string
	Method    string
	Path      string
	Templates []string
}

// Scopes are the scopes op needs with params, each hole escaped.
func (op Operation) Scopes(params map[string]string) []string {
	scopes := make([]string, len(op.Templates))
	for i, template := range op.Templates {
		scopes[i] = Fill(template, params)
	}
	return scopes
}

// Request is an operation's input, as each generated request struct gives
// it.
type Request interface {
	Operation() Operation
	Params() map[string]string
	Query() url.Values
	Body() ([]byte, error)
}

// Client sends request and decodes a 2xx body into response; a non-2xx
// answer is an *APIError, a transport failure its own error.
type Client interface {
	Do(ctx context.Context, request Request, response any) error
}

// Streamer opens one answer (a window) of request's stream of events,
// with cursor, when not empty, as its `cursor` query (in place of the
// request's own). Open[T] reconnects it into a Stream.
type Streamer interface {
	Open(ctx context.Context, request Request, cursor string) (Events, error)
}

// Events are one answer's raw events; Next returns io.EOF at its end.
type Events interface {
	Next() (RawEvent, error)
	Close() error
}

// RawEvent is one server-sent event, as read.
type RawEvent struct {
	Event string
	Data  string
	ID    string
}

// The reasons an error answers with, as errors.Is targets of an *APIError.
// ErrMissingScope is also what HTTPClient returns, without calling, when
// its session cannot cover a call.
var (
	ErrMissingScope     = errors.New("hashinbox: missing scope")
	ErrTOTPRequired     = errors.New("hashinbox: two-factor code required")
	ErrWrongCredentials = errors.New("hashinbox: wrong credentials")
	ErrSessionExpired   = errors.New("hashinbox: session expired")
)

// APIError is a non-2xx answer: its status, the API's error body decoded
// (empty when the body is not one), and the body as sent.
type APIError struct {
	Status   int
	Kind     Kind
	Message  string
	Reason   string
	Metadata map[string]string
	Raw      []byte
}

// Error is its status, kind, reason and message.
func (e *APIError) Error() string {
	text := fmt.Sprintf("hashinbox: HTTP %d", e.Status)
	if e.Kind != "" {
		text += " " + string(e.Kind)
	}
	if e.Reason != "" {
		text += " (" + e.Reason + ")"
	}
	if e.Message != "" {
		text += ": " + e.Message
	}
	return text
}

// Is reports whether target is the Err… of its reason.
func (e *APIError) Is(target error) bool {
	switch target {
	case ErrMissingScope:
		return e.Reason == "missing_scope"
	case ErrTOTPRequired:
		return e.Reason == "totp_required"
	case ErrWrongCredentials:
		return e.Reason == "wrong_credentials"
	case ErrSessionExpired:
		return e.Reason == "session_expired"
	}
	return false
}

// newAPIError is the error of status with raw as its body.
func newAPIError(status int, raw []byte) *APIError {
	e := &APIError{Status: status, Raw: raw}
	var body Error
	if json.Unmarshal(raw, &body) == nil {
		e.Kind, e.Message, e.Metadata = body.Kind, body.Message, body.Metadata
		if body.Reason != nil {
			e.Reason = *body.Reason
		}
	}
	return e
}

// Path is request's path, each {name} its parameter, escaped.
func Path(request Request) string {
	return fill(request.Operation().Path, request.Params(), url.PathEscape)
}

// HTTPClient is the default Client and Streamer: net/http, JSON, a bearer
// session, the scope check before each call. Safe for concurrent use.
type HTTPClient struct {
	base      string
	http      *http.Client
	timeout   time.Duration
	userAgent string
	check     bool

	backoff    time.Duration // a stream's first wait before reconnecting after an error
	maxBackoff time.Duration // and its longest

	mu      sync.Mutex
	session *Session
	secret  *Secret
	scopes  Scopes // what secret signs for
	known   bool   // whether scopes are known: given, or read from the key
	given   bool   // WithSecretScopes
	ttl     time.Duration
}

// Option configures an HTTPClient.
type Option func(*HTTPClient)

// renewBefore is how long before its expiry a signed session is replaced.
const renewBefore = 60 * time.Second

// NewClient is a client of the server at baseURL (`https://pay.example`).
func NewClient(baseURL string, opts ...Option) *HTTPClient {
	c := &HTTPClient{
		base:      strings.TrimRight(baseURL, "/"),
		http:      http.DefaultClient,
		userAgent: "hashinbox-go",
		check:     true,
		ttl:       defaultTTL,

		backoff:    500 * time.Millisecond,
		maxBackoff: 8 * time.Second,
	}
	for _, opt := range opts {
		opt(c)
	}
	return c
}

// WithSession sends session's JWT as the bearer.
func WithSession(session Session) Option {
	return func(c *HTTPClient) { c.session = &session }
}

// WithSecret signs the client's own sessions with secret and sends them;
// the secret itself never leaves. Each lasts 10 minutes (WithSessionTTL)
// and holds the key's scopes (read once from GET /v1/session with a
// session of no scopes) or WithSecretScopes'. A new one is signed about
// 60 s before the last expires, and after a 401 session_expired (the call
// retried once). The server accepts a session while its clock is at most
// 60 s behind the `iat` this machine's clock gives and before `exp`: keep
// the clock in sync.
func WithSecret(secret Secret) Option {
	return func(c *HTTPClient) { c.secret = &secret }
}

// WithSecretScopes is what WithSecret's sessions hold, covered by the
// key's.
func WithSecretScopes(scopes Scopes) Option {
	return func(c *HTTPClient) { c.scopes, c.known, c.given = scopes, true, true }
}

// WithSessionTTL is how long WithSecret's sessions last, more than 60 s,
// at most MaxTTL.
func WithSessionTTL(ttl time.Duration) Option {
	return func(c *HTTPClient) { c.ttl = ttl }
}

// WithHTTPClient sends through client.
func WithHTTPClient(client *http.Client) Option {
	return func(c *HTTPClient) { c.http = client }
}

// WithTimeout bounds each Do and Login call; a stream is bounded by its
// ctx only.
func WithTimeout(timeout time.Duration) Option {
	return func(c *HTTPClient) { c.timeout = timeout }
}

// WithUserAgent is the User-Agent sent.
func WithUserAgent(userAgent string) Option {
	return func(c *HTTPClient) { c.userAgent = userAgent }
}

// WithStreamBackoff is how long a Stream waits before reconnecting after
// an error: first, doubling up to most (500 ms and 8 s by default).
func WithStreamBackoff(first, most time.Duration) Option {
	return func(c *HTTPClient) { c.backoff, c.maxBackoff = first, most }
}

// streamBackoff is WithStreamBackoff's.
func (c *HTTPClient) streamBackoff() (time.Duration, time.Duration) {
	return c.backoff, c.maxBackoff
}

// WithoutScopeCheck sends every call, leaving the scope check to the
// server.
func WithoutScopeCheck() Option {
	return func(c *HTTPClient) { c.check = false }
}

// Session is the session it sends; false when none.
func (c *HTTPClient) Session() (Session, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.session == nil {
		return Session{}, false
	}
	return *c.session, true
}

// Do sends request and decodes a 2xx body into response (nil: dropped).
func (c *HTTPClient) Do(ctx context.Context, request Request, response any) error {
	ctx, cancel := c.bound(ctx)
	defer cancel()
	answer, err := c.exchange(ctx, request, "application/json", "")
	if err != nil {
		return err
	}
	return decode(answer, response)
}

// Open opens one answer of request's stream of server-sent events, read
// until its end, ctx's or Close; with cursor, when not empty, as its
// `cursor` query.
func (c *HTTPClient) Open(ctx context.Context, request Request, cursor string) (Events, error) {
	answer, err := c.exchange(ctx, request, "text/event-stream", cursor)
	if err != nil {
		return nil, err
	}
	lines := bufio.NewScanner(answer.Body)
	lines.Buffer(make([]byte, 0, 64<<10), 16<<20)
	return &sse{body: answer.Body, lines: lines}, nil
}

// bound is ctx bounded by WithTimeout's timeout.
func (c *HTTPClient) bound(ctx context.Context) (context.Context, context.CancelFunc) {
	if c.timeout <= 0 {
		return ctx, func() {}
	}
	return context.WithTimeout(ctx, c.timeout)
}

// LoginRequest is a login: email and password, or the recovery passphrase;
// and the two-factor code once it is on.
type LoginRequest struct {
	Email      string
	Password   string
	Passphrase string
	TOTP       string
}

// Login logs in and sends the user's session from now on, in place of any
// session or secret. A wrong login is ErrWrongCredentials; a right one
// without its code, ErrTOTPRequired (errors.Is).
func (c *HTTPClient) Login(ctx context.Context, login LoginRequest) (Session, error) {
	optional := func(text string) *string {
		if text == "" {
			return nil
		}
		return &text
	}
	request := SessionCreateRequest{CreateSessionRequest{
		Email:      &login.Email,
		Password:   optional(login.Password),
		Passphrase: optional(login.Passphrase),
		Totp:       optional(login.TOTP),
	}}
	ctx, cancel := c.bound(ctx)
	defer cancel()
	answer, err := c.attempt(ctx, nil, request, "application/json", "")
	if err != nil {
		return Session{}, err
	}
	var issued SessionCreateResponse
	if err := decode(answer, &issued); err != nil {
		return Session{}, err
	}
	session, err := ParseSession(issued.Token)
	if err != nil {
		return Session{}, err
	}
	c.mu.Lock()
	c.session, c.secret = &session, nil
	c.mu.Unlock()
	return session, nil
}

// Logout forgets its session; with all, the server first refuses every
// session its issuer (the user, or the key) issued until now.
func (c *HTTPClient) Logout(ctx context.Context, all bool) error {
	var err error
	if all {
		err = c.Do(ctx, SessionDeleteRequest{All: &all}, nil)
	}
	c.mu.Lock()
	c.session = nil
	c.mu.Unlock()
	return err
}

// exchange sends request with its session, signing a new one and retrying
// once after a 401 session_expired when it signs its own; a 2xx answer, its
// body to close.
func (c *HTTPClient) exchange(ctx context.Context, request Request, accept, cursor string) (*http.Response, error) {
	session, err := c.bearer(ctx, "")
	if err != nil {
		return nil, err
	}
	answer, err := c.attempt(ctx, session, request, accept, cursor)
	if session == nil || !c.signs() || !errors.Is(err, ErrSessionExpired) {
		return answer, err
	}
	session, err = c.bearer(ctx, session.Token)
	if err != nil {
		return nil, err
	}
	return c.attempt(ctx, session, request, accept, cursor)
}

// signs reports whether it signs its own sessions.
func (c *HTTPClient) signs() bool {
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.secret != nil
}

// bearer is the session to send: the one given, or a signed one, signed
// anew when it is near expiry or is failed (refused as expired).
func (c *HTTPClient) bearer(ctx context.Context, failed string) (*Session, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.secret == nil {
		return c.session, nil
	}
	current := c.session
	if current != nil && current.Token != failed && now().Add(renewBefore).Before(current.ExpiresAt) {
		return current, nil
	}
	if failed != "" && !c.given {
		c.known = false // the key may have been narrowed: read its scopes again
	}
	if !c.known {
		scopes, err := c.keyScopes(ctx)
		if err != nil {
			return nil, err
		}
		c.scopes, c.known = scopes, true
	}
	session, err := c.secret.Issue(WithScopes(c.scopes), WithTTL(c.ttl))
	if err != nil {
		return nil, err
	}
	c.session = &session
	return c.session, nil
}

// keyScopes are the scopes of c's key, read with a session of none.
func (c *HTTPClient) keyScopes(ctx context.Context) (Scopes, error) {
	probe, err := c.secret.Issue(WithTTL(time.Minute))
	if err != nil {
		return nil, err
	}
	answer, err := c.attempt(ctx, &probe, SessionGetRequest{}, "application/json", "")
	if err != nil {
		return nil, err
	}
	var read SessionGetResponse
	if err := decode(answer, &read); err != nil {
		return nil, err
	}
	if read.Key == nil {
		return nil, errors.New("hashinbox: the secret's session names no key")
	}
	return ParseScopes(read.Key.Scopes)
}

// attempt sends request once with session (nil: no bearer, no scope
// check), with cursor, when not empty, as its `cursor` query; a 2xx
// answer, or the error.
func (c *HTTPClient) attempt(ctx context.Context, session *Session, request Request, accept, cursor string) (*http.Response, error) {
	op := request.Operation()
	if c.check && session != nil {
		if scope, ok := session.missing(op, request.Params()); !ok {
			return nil, fmt.Errorf("%w: %s needs %s", ErrMissingScope, op.ID, scope)
		}
	}
	target := c.base + Path(request)
	query := request.Query()
	if cursor != "" {
		if query == nil {
			query = url.Values{}
		}
		query.Set("cursor", cursor)
	}
	if len(query) > 0 {
		target += "?" + query.Encode()
	}
	body, err := request.Body()
	if err != nil {
		return nil, err
	}
	var reader io.Reader
	if body != nil {
		reader = bytes.NewReader(body)
	}
	outgoing, err := http.NewRequestWithContext(ctx, op.Method, target, reader)
	if err != nil {
		return nil, err
	}
	if body != nil {
		outgoing.Header.Set("Content-Type", "application/json")
	}
	if session != nil {
		outgoing.Header.Set("Authorization", "Bearer "+session.Token)
	}
	outgoing.Header.Set("Accept", accept)
	outgoing.Header.Set("User-Agent", c.userAgent)
	answer, err := c.http.Do(outgoing)
	if err != nil {
		return nil, err
	}
	if answer.StatusCode/100 == 2 {
		return answer, nil
	}
	defer answer.Body.Close()
	raw, err := io.ReadAll(answer.Body)
	if err != nil {
		return nil, err
	}
	return nil, newAPIError(answer.StatusCode, raw)
}

// decode reads answer's body into response, then closes it. An empty body
// or a nil response decodes nothing; a response of one field `Value` (a
// generated response of no model) is decoded into that field.
func decode(answer *http.Response, response any) error {
	defer answer.Body.Close()
	raw, err := io.ReadAll(answer.Body)
	if err != nil || response == nil || len(bytes.TrimSpace(raw)) == 0 {
		return err
	}
	if v := reflect.ValueOf(response); v.Kind() == reflect.Pointer && v.Elem().Kind() == reflect.Struct {
		t := v.Elem().Type()
		if t.NumField() == 1 && t.Field(0).Name == "Value" && !t.Field(0).Anonymous {
			response = v.Elem().Field(0).Addr().Interface()
		}
	}
	return json.Unmarshal(raw, response)
}

// sse reads server-sent events off a body.
type sse struct {
	body  io.ReadCloser
	lines *bufio.Scanner
}

// Next is the next event with data; io.EOF at the end of the body.
func (s *sse) Next() (RawEvent, error) {
	var event RawEvent
	var data []string
	for s.lines.Scan() {
		line := s.lines.Text()
		if line == "" {
			if data == nil {
				event = RawEvent{}
				continue
			}
			if event.Event == "" {
				event.Event = "message"
			}
			event.Data = strings.Join(data, "\n")
			return event, nil
		}
		field, value, _ := strings.Cut(line, ":")
		value = strings.TrimPrefix(value, " ")
		switch field {
		case "event":
			event.Event = value
		case "data":
			data = append(data, value)
		case "id":
			event.ID = value
		}
	}
	if err := s.lines.Err(); err != nil {
		return RawEvent{}, err
	}
	return RawEvent{}, io.EOF
}

// Close closes the body.
func (s *sse) Close() error {
	return s.body.Close()
}

// Stream is a stream of typed events over as many answers as it takes:
// the server ends each after its window (`event: close`, not delivered)
// and Stream reconnects at once, sending the last event's id as its
// `cursor` query; after a transport error, a cut answer or a 5xx/429 it
// waits first (WithStreamBackoff). It ends at an answer's end without
// `close` (the stream is done, io.EOF), at any other error answer (an
// *APIError, the token checked anew on each reconnect), on Stop (io.EOF),
// or when its ctx ends (ctx's error). Without Stop or ctx's end it keeps
// reconnecting. Next is for one goroutine; Stop and Cursor for any.
type Stream[T any] struct {
	ctx      context.Context
	cancel   context.CancelFunc
	streamer Streamer
	request  Request
	decode   func(RawEvent) (T, bool, error)
	first    time.Duration // the first wait after an error
	most     time.Duration // the longest

	mu      sync.Mutex
	events  Events // the current answer; nil between answers
	lastID  string
	stopped bool
	done    bool
}

// Next returns the next event this client knows, skipping any other,
// reconnecting as it must; io.EOF at the end.
func (s *Stream[T]) Next() (T, error) {
	var zero T
	var wait time.Duration
	for {
		events, done := s.current()
		if done {
			return zero, io.EOF
		}
		if err := s.ctx.Err(); err != nil {
			s.drop()
			if s.isStopped() {
				return zero, io.EOF
			}
			return zero, err
		}
		if events == nil {
			if !s.sleep(wait) {
				continue
			}
			opened, err := s.streamer.Open(s.ctx, s.request, s.Cursor())
			if err != nil {
				if s.ctx.Err() == nil && !retryable(err) {
					s.finish()
					return zero, err
				}
				wait = s.longer(wait)
				continue
			}
			wait = 0
			if !s.hold(opened) {
				continue
			}
			events = opened
		}
		raw, err := events.Next()
		if errors.Is(err, io.EOF) {
			s.finish()
			return zero, io.EOF
		}
		if err != nil {
			s.drop()
			wait = s.longer(wait)
			continue
		}
		if raw.ID != "" {
			s.mu.Lock()
			s.lastID = raw.ID
			s.mu.Unlock()
		}
		if raw.Event == "close" {
			s.drop()
			wait = 0
			continue
		}
		event, ok, err := s.decode(raw)
		if ok || err != nil {
			return event, err
		}
	}
}

// Cursor is the id of the last event read: the `cursor` a reconnect
// resumes from; empty before the first.
func (s *Stream[T]) Cursor() string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.lastID
}

// Stop ends the stream for good: its answer closed, no reconnect; Next
// then returns io.EOF.
func (s *Stream[T]) Stop() {
	s.mu.Lock()
	s.stopped = true
	s.mu.Unlock()
	s.cancel()
	s.drop()
}

// Close is Stop.
func (s *Stream[T]) Close() error {
	s.Stop()
	return nil
}

// current is its answer, and whether it is done.
func (s *Stream[T]) current() (Events, bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.events, s.done
}

// hold keeps events as its answer; false (events closed) once stopped.
func (s *Stream[T]) hold(events Events) bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	if s.stopped {
		events.Close()
		return false
	}
	s.events = events
	return true
}

// drop closes its answer, if any.
func (s *Stream[T]) drop() {
	s.mu.Lock()
	events := s.events
	s.events = nil
	s.mu.Unlock()
	if events != nil {
		events.Close()
	}
}

// finish ends it: done, its answer closed.
func (s *Stream[T]) finish() {
	s.drop()
	s.mu.Lock()
	s.done = true
	s.mu.Unlock()
	s.cancel()
}

func (s *Stream[T]) isStopped() bool {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.stopped
}

// sleep waits wait, or less if ctx ends first (then false).
func (s *Stream[T]) sleep(wait time.Duration) bool {
	if wait <= 0 {
		return true
	}
	timer := time.NewTimer(wait)
	defer timer.Stop()
	select {
	case <-timer.C:
		return true
	case <-s.ctx.Done():
		return false
	}
}

// longer is the wait after wait: the first, then doubled up to the most.
func (s *Stream[T]) longer(wait time.Duration) time.Duration {
	if wait <= 0 {
		return s.first
	}
	return min(wait*2, s.most)
}

// retryable reports whether a failed (re)connect is worth another after a
// wait: a transport error, or a 5xx or 429 answer.
func retryable(err error) bool {
	var answer *APIError
	if errors.As(err, &answer) {
		return answer.Status >= 500 || answer.Status == http.StatusTooManyRequests
	}
	var transport *url.Error
	return errors.As(err, &transport)
}

// Open opens request with s as a Stream, its events decoded as T: the
// first answer opened here, its error returned.
func Open[T any, P decoder[T]](ctx context.Context, s Streamer, request Request) (*Stream[T], error) {
	ctx, cancel := context.WithCancel(ctx)
	events, err := s.Open(ctx, request, "")
	if err != nil {
		cancel()
		return nil, err
	}
	decode := func(raw RawEvent) (T, bool, error) {
		var event T
		ok, err := P(&event).decode(raw)
		return event, ok, err
	}
	first, most := 500*time.Millisecond, 8*time.Second
	if backoff, ok := s.(interface {
		streamBackoff() (time.Duration, time.Duration)
	}); ok {
		first, most = backoff.streamBackoff()
	}
	return &Stream[T]{
		ctx: ctx, cancel: cancel, streamer: s, request: request, decode: decode,
		first: first, most: most, events: events,
	}, nil
}

func (e *RawEvent) decode(raw RawEvent) (bool, error) {
	*e = raw
	return true, nil
}

type decoder[T any] interface {
	*T
	decode(raw RawEvent) (bool, error)
}

// jsonValue is any JSON value, kept as sent.
type jsonValue = json.RawMessage

// text is v as query or path text.
func text(v any) string {
	if s, ok := v.(string); ok {
		return s
	}
	return fmt.Sprint(v)
}

// decodeData decodes raw's data into v.
func decodeData(raw RawEvent, v any) error {
	return json.Unmarshal([]byte(raw.Data), v)
}

// encodeJSON is v as JSON.
func encodeJSON(v any) ([]byte, error) {
	return json.Marshal(v)
}

// decodeStrict reports whether data decodes into v with no field v lacks.
func decodeStrict(data []byte, v any) bool {
	decoder := json.NewDecoder(bytes.NewReader(data))
	decoder.DisallowUnknownFields()
	return decoder.Decode(v) == nil
}

// noVariant is the error of a union no variant of fits.
func noVariant(union string) error {
	return errors.New("cligen: no variant of " + union + " fits")
}
