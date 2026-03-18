// wail-metrics queries session metrics from a WAIL signaling server.
//
// Usage:
//
//	wail-metrics [flags]
//	wail-metrics -server https://signal.wail.live -room my-room
//	wail-metrics -json
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"text/tabwriter"
)

type directionMetrics struct {
	FramesExpected uint64 `json:"frames_expected"`
	FramesReceived uint64 `json:"frames_received"`
	FramesDropped  uint64 `json:"frames_dropped"`
}

type sessionJSON struct {
	ID        string                       `json:"id"`
	Room      string                       `json:"room"`
	StartedAt string                       `json:"started_at"`
	EndedAt   *string                      `json:"ended_at,omitempty"`
	Duration  string                       `json:"duration"`
	Phase     string                       `json:"phase"`
	Peers     []string                     `json:"peers"`
	Joining   map[string]*directionMetrics `json:"joining"`
	Playing   map[string]*directionMetrics `json:"playing"`
}

type metricsResponse struct {
	Active    []sessionJSON `json:"active"`
	Completed []sessionJSON `json:"completed"`
}

func main() {
	server := flag.String("server", "https://signal.wail.live", "Signaling server URL")
	room := flag.String("room", "", "Filter by room name")
	jsonOut := flag.Bool("json", false, "Output raw JSON")
	flag.Parse()

	u, err := url.Parse(*server)
	if err != nil {
		fmt.Fprintf(os.Stderr, "invalid server URL: %v\n", err)
		os.Exit(1)
	}
	// Normalize to HTTP(S)
	switch u.Scheme {
	case "ws":
		u.Scheme = "http"
	case "wss":
		u.Scheme = "https"
	}
	u.Path = strings.TrimRight(u.Path, "/") + "/metrics"
	if *room != "" {
		q := u.Query()
		q.Set("room", *room)
		u.RawQuery = q.Encode()
	}

	resp, err := http.Get(u.String())
	if err != nil {
		fmt.Fprintf(os.Stderr, "request failed: %v\n", err)
		os.Exit(1)
	}
	defer resp.Body.Close()

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		fmt.Fprintf(os.Stderr, "read body: %v\n", err)
		os.Exit(1)
	}
	if resp.StatusCode != 200 {
		fmt.Fprintf(os.Stderr, "server returned %d: %s\n", resp.StatusCode, string(body))
		os.Exit(1)
	}

	if *jsonOut {
		// Pretty-print JSON
		var v any
		if err := json.Unmarshal(body, &v); err != nil {
			fmt.Fprintf(os.Stderr, "parse response: %v\n", err)
			os.Exit(1)
		}
		enc := json.NewEncoder(os.Stdout)
		enc.SetIndent("", "  ")
		enc.Encode(v)
		return
	}

	var metrics metricsResponse
	if err := json.Unmarshal(body, &metrics); err != nil {
		fmt.Fprintf(os.Stderr, "parse response: %v\n", err)
		os.Exit(1)
	}

	if len(metrics.Active) == 0 && len(metrics.Completed) == 0 {
		fmt.Println("No sessions found.")
		return
	}

	if len(metrics.Active) > 0 {
		fmt.Printf("=== Active Sessions (%d) ===\n\n", len(metrics.Active))
		for _, s := range metrics.Active {
			printSession(s)
		}
	}

	if len(metrics.Completed) > 0 {
		fmt.Printf("=== Completed Sessions (%d) ===\n\n", len(metrics.Completed))
		for _, s := range metrics.Completed {
			printSession(s)
		}
	}
}

func printSession(s sessionJSON) {
	fmt.Printf("Session: %s\n", s.ID)
	fmt.Printf("  Room:     %s\n", s.Room)
	fmt.Printf("  Phase:    %s\n", s.Phase)
	fmt.Printf("  Duration: %s\n", s.Duration)
	fmt.Printf("  Peers:    %s\n", strings.Join(s.Peers, ", "))
	fmt.Printf("  Started:  %s\n", s.StartedAt)
	if s.EndedAt != nil {
		fmt.Printf("  Ended:    %s\n", *s.EndedAt)
	}

	if len(s.Joining) > 0 {
		fmt.Printf("\n  Joining phase:\n")
		printDirections(s.Joining)
	}
	if len(s.Playing) > 0 {
		fmt.Printf("\n  Playing phase:\n")
		printDirections(s.Playing)
	}
	fmt.Println()
}

func printDirections(dirs map[string]*directionMetrics) {
	w := tabwriter.NewWriter(os.Stdout, 0, 0, 2, ' ', 0)
	fmt.Fprintf(w, "    DIRECTION\tEXPECTED\tRECEIVED\tDROPPED\tDROP %%\n")
	for dir, m := range dirs {
		pct := 0.0
		if m.FramesExpected > 0 {
			pct = float64(m.FramesDropped) / float64(m.FramesExpected) * 100
		}
		fmt.Fprintf(w, "    %s\t%d\t%d\t%d\t%.1f%%\n",
			dir, m.FramesExpected, m.FramesReceived, m.FramesDropped, pct)
	}
	w.Flush()
}
