// nanobot-gateway: Go stdlib-only channel gateway for nanobot-rs core.
// - Telegram long-polling -> CORE /api/run (session per chat)
// - cron: NANOBOT_CRON="HH:MM prompt|HH:MM prompt" daily -> CORE /api/run
// - webhook POST /hook {text,session_id} -> CORE (generic channel, feeds cron result too)
// - serves WebUI at GET / (webui/index.html on disk)
package main

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

var coreURL = envOr("NANOBOT_CORE", "http://127.0.0.1:8090")

// fresh-review: no timeout on outbound calls once let a hung core pin goroutines forever
var coreClient = &http.Client{Timeout: 320 * time.Second} // > core's 300s LLM timeout
var healthClient = &http.Client{Timeout: 10 * time.Second}

func jsonErr(w http.ResponseWriter, code int, msg string) {
	// fresh-review: http.Error sends text/plain while the body is JSON-shaped —
	// strict JSON clients choke; emit the right Content-Type ourselves.
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	fmt.Fprintf(w, "{\"error\":%q}\n", msg)
}

func envOr(k, def string) string {
	if v := os.Getenv(k); v != "" {
		return v
	}
	return def
}

func coreRun(prompt, session string) string {
	body, _ := json.Marshal(map[string]string{"prompt": prompt, "session_id": session})
	for attempt := 0; attempt < 3; attempt++ {
		resp, err := coreClient.Post(coreURL+"/api/run", "application/json", bytes.NewReader(body))
		if err != nil {
			// fresh-review: only retry when the request never left us (dial phase);
			// a broken response means the core may have run the agent once already,
			// and re-running non-idempotent tool calls again would double the effects
			var oe *net.OpError
			if errors.As(err, &oe) && oe.Op == "dial" && attempt < 2 {
				log.Printf("core attempt %d: %v", attempt, err)
				time.Sleep(4 * time.Second)
				continue
			}
			return "(core unreachable: " + err.Error() + ")"
		}
		raw, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		var out struct {
			Text string `json:"text"`
		}
		if json.Unmarshal(raw, &out) == nil && out.Text != "" {
			return out.Text
		}
		return string(raw)
	}
	return "(core unreachable)"
}

// tgTruncate cut at a rune boundary (byte slicing splits multi-byte UTF-8)
func tgTruncate(text string) string {
	r := []rune(text)
	const maxRunes = 3900 // Telegram cap 4096; margin for the marker below
	if len(r) <= maxRunes {
		return text
	}
	return string(r[:maxRunes]) + "\n[truncated]"
}

// ---- Telegram ----
// fresh-review: the poll offset used to live only in RAM — a gateway restart re-fed
// 24h of old updates to the core (duplicate runs, duplicate replies). Persist it.
func tgOffsetPath() string { return envOr("NANOBOT_TG_OFFSET_FILE", "tg-offset.txt") }

func loadOffset() int64 {
	b, err := os.ReadFile(tgOffsetPath())
	if err != nil {
		return 0
	}
	n, err := strconv.ParseInt(strings.TrimSpace(string(b)), 10, 64)
	if err != nil {
		return 0
	}
	return n
}

func saveOffset(n int64) {
	_ = os.WriteFile(tgOffsetPath(), []byte(strconv.FormatInt(n, 10)), 0o600)
}

func tgPoll(token string) {
	base := "https://api.telegram.org/bot" + token
	client := &http.Client{Timeout: 70 * time.Second}
	offset := loadOffset()
	for {
		var up struct {
			OK     bool `json:"ok"`
			Result []struct {
				UpdateID int64 `json:"update_id"`
				Message  *struct {
					Chat struct {
						ID int64 `json:"id"`
					} `json:"chat"`
					Text string `json:"text"`
				} `json:"message"`
			} `json:"result"`
		}
		updates := url.QueryEscape(`["message"]`)
		respURL := fmt.Sprintf("%s/getUpdates?timeout=60&offset=%d&allowed_updates=%s", base, offset, updates)
		resp, err := client.Get(respURL)
		if err != nil {
			log.Printf("tg poll: %v", err)
			time.Sleep(5 * time.Second)
			continue
		}
		raw, _ := io.ReadAll(resp.Body)
		resp.Body.Close()
		if json.Unmarshal(raw, &up) != nil || !up.OK {
			time.Sleep(5 * time.Second)
			continue
		}
		for _, u := range up.Result {
			offset = u.UpdateID + 1
			saveOffset(offset)
			if u.Message == nil || u.Message.Text == "" {
				continue
			}
			chatID := u.Message.Chat.ID
			if !tgAllowedChat(chatID) {
				log.Printf("tg: chat %d not whitelisted, drop", chatID)
				continue
			}
			go func(text string, id int64) {
				answer := coreRun(text, fmt.Sprintf("tg-%d", id))
				tgSend(base, client, id, answer)
			}(u.Message.Text, chatID)
		}
	}
}

func tgSend(base string, client *http.Client, chat int64, text string) {
	text = tgTruncate(text)
	body, _ := json.Marshal(map[string]any{"chat_id": chat, "text": text})
	resp, err := client.Post(base+"/sendMessage", "application/json", bytes.NewReader(body))
	if err != nil {
		log.Printf("tg send: %v", err)
		return
	}
	resp.Body.Close()
}

// TG whitelist: NANOBOT_TG_ALLOW="123,456" (comma chat ids); unset = allow all
var tgAllowed = map[int64]bool{}
var tgAllowSet bool

func tgAllowedChat(id int64) bool { return !tgAllowSet || tgAllowed[id] }

// ---- cron: "09:00 morning briefing|21:30 evening digest" ----
func cronLoop() {
	spec := os.Getenv("NANOBOT_CRON")
	if spec == "" {
		return
	}
	type job struct {
		hh, mm string
		prompt string
	}
	var jobs []job
	for _, part := range strings.Split(spec, "|") {
		f := strings.SplitN(strings.TrimSpace(part), " ", 2)
		if len(f) != 2 || len(strings.Split(f[0], ":")) != 2 {
			log.Printf("cron skip malformed spec %q", part)
			continue
		}
		hm := strings.Split(f[0], ":")
		if hh, mm := atoi(hm[0]), atoi(hm[1]); hh < 0 || hh > 23 || mm < 0 || mm > 59 {
			log.Printf("cron skip invalid time %q", f[0])
			continue
		}
		jobs = append(jobs, job{hm[0], hm[1], f[1]})
	}
	sentToday := map[string]bool{}
	lastKey := ""
	for {
		now := time.Now()
		key := fmt.Sprintf("%d-%02d-%02d", now.Year(), now.Month(), now.Day())
		if key != lastKey { // fresh-review: per-day fired-once markers would otherwise grow forever
			sentToday = map[string]bool{}
			lastKey = key
		}
		for _, j := range jobs {
			fire := fmt.Sprintf("%02d:%02d", atoi(j.hh), atoi(j.mm))
			id := fmt.Sprintf("%s-%s", fire, key) // separator: ":30x" and "3:0x" must not collide
			// fresh-review: a suspended/sleeping host used to swallow the job's minute for
			// the whole day; fire once late rather than never
			nowMin := now.Hour()*60 + now.Minute()
			missed := atoi(j.hh)*60 + atoi(j.mm)
			if !sentToday[id] && nowMin >= missed && nowMin < missed+30 {
				sentToday[id] = true
				log.Printf("cron fire %s (late=%v)", fire, nowMin != missed)
				go func(prompt string) { _ = coreRun(prompt, "cron") }(j.prompt)
			}
		}
		time.Sleep(30 * time.Second)
	}
}

func atoi(s string) int {
	// fresh-review: an empty field used to parse as 0, sneaking "0:" specs into a
	// 00:00 firing; treat any empty/non-numeric field as invalid (-1)
	s = strings.TrimSpace(s)
	if s == "" {
		return -1
	}
	n := 0
	for _, c := range s {
		if c < '0' || c > '9' {
			return -1
		}
		n = n*10 + int(c-'0')
	}
	return n
}

func fileExists(p string) bool {
	st, err := os.Stat(p)
	return err == nil && !st.IsDir()
}

// fresh-review: `{"text":%q}` is Go quoting, not JSON — invalid UTF-8 rounds-trip
// as \xNN escapes that JSON.parse rejects; encoding/json always yields valid JSON.
func writeText(w http.ResponseWriter, text string) {
	body, _ := json.Marshal(map[string]string{"text": text})
	w.Header().Set("Content-Type", "application/json")
	w.Write(body)
}

func main() {
	log.SetFlags(log.LstdFlags | log.LUTC)
	if s := os.Getenv("NANOBOT_TG_ALLOW"); s != "" {
		tgAllowSet = true
		for _, f := range strings.Split(s, ",") {
			if id := atoi(strings.TrimSpace(f)); id >= 0 {
				tgAllowed[int64(id)] = true
			}
		}
		log.Printf("tg whitelist: %d chats", len(tgAllowed))
	}
	go func() {
		if t := os.Getenv("TELEGRAM_BOT_TOKEN"); t != "" {
			tgPoll(t)
		}
	}()
	go cronLoop()

	http.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/" {
			http.NotFound(w, r)
			return
		}
		// fresh-review: the WebUI path was CWD-relative; running ./gateway/gateway-bin
		// from the repo root 404'd every page — resolve next to the executable
		page := "webui/index.html"
		if exe, err := os.Executable(); err == nil && filepath.Base(exe) == "gateway-bin" {
			if p := filepath.Join(filepath.Dir(exe), "webui", "index.html"); fileExists(p) {
				page = p
			}
		}
		http.ServeFile(w, r, page)
	})
	// health probe for the WebUI (forward to core)
	http.HandleFunc("/api/health", func(w http.ResponseWriter, r *http.Request) {
		resp, err := healthClient.Get(coreURL + "/health")
		if err != nil {
			jsonErr(w, http.StatusBadGateway, "core unreachable")
			return
		}
		defer resp.Body.Close()
		body, _ := io.ReadAll(io.LimitReader(resp.Body, 1<<16))
		w.Header().Set("Content-Type", "application/json")
		w.Write(body)
	})
	// reverse-proxy the core agent API for WebUI (same-origin, no CORS headaches)
	http.HandleFunc("/api/run", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodPost {
			jsonErr(w, http.StatusMethodNotAllowed, "POST only")
			return
		}
		// fresh-review: a text/plain POST is a CORS "simple request", so without this
		// gate /api/run (and through it the core's exec tool) was reachable from any
		// browsed website without a preflight
		if !strings.Contains(r.Header.Get("Content-Type"), "application/json") {
			jsonErr(w, http.StatusUnsupportedMediaType, "json only")
			return
		}
		var in struct {
			Prompt    string `json:"prompt"`
			SessionID string `json:"session_id"`
		}
		if json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&in) != nil || in.Prompt == "" {
			jsonErr(w, http.StatusBadRequest, "need prompt")
			return
		}
		// fresh-review: anonymous callers used to share the fixed "web" session and
		// read each other's context — generate a private one when none is supplied
		if in.SessionID == "" {
			in.SessionID = fmt.Sprintf("web-%d", time.Now().UnixNano()%1_000_000_000)
		}
		writeText(w, coreRun(in.Prompt, in.SessionID))
	})
	// generic inbound webhook: POST /hook {"text":"...","session_id":"..."}
	http.HandleFunc("/hook", func(w http.ResponseWriter, r *http.Request) {
		var in struct {
			Text      string `json:"text"`
			SessionID string `json:"session_id"`
		}
		if !strings.Contains(r.Header.Get("Content-Type"), "application/json") {
			jsonErr(w, http.StatusUnsupportedMediaType, "json only")
			return
		}
		if json.NewDecoder(io.LimitReader(r.Body, 1<<20)).Decode(&in) != nil || in.Text == "" {
			jsonErr(w, http.StatusBadRequest, "need text")
			return
		}
		if in.SessionID == "" {
			in.SessionID = fmt.Sprintf("hook-%d", time.Now().UnixNano()%1_000_000_000)
		}
		writeText(w, coreRun(in.Text, in.SessionID))
	})
	// default bind to loopback only: /hook & /api/run are unauthenticated (opt out with NANOBOT_BIND=0.0.0.0)
	bind := envOr("NANOBOT_BIND", "127.0.0.1")
	log.Printf("gateway on %s:%s core=%s", bind, envOr("NANOBOT_PORT", "8081"), coreURL)
	log.Fatal(http.ListenAndServe(bind+":"+envOr("NANOBOT_PORT", "8081"), nil))
}
