// Contender: chromedp (native Go CDP client, github.com/chromedp/chromedp).
//
// Same canonical session as the other contenders:
//   session — launch, goto A, title/count/extract evals, fill, click,
//             visible-count eval, screenshot, new tab -> goto B,
//             title/row-count evals, close
//   eval    — warm eval round-trip micro: goto A, then 200x eval title
//             (per-op latencies printed as JSON lines on stdout)
//   cold    — launch, new page, one eval, close (cold-start micro)
//
// Env: CHROME_BIN (chrome executable, required),
//      LADDER_BASE (fixture server base URL),
//      LADDER_EXTRACT_OUT, LADDER_SHOT_OUT (session mode),
//      LADDER_N_EVAL (eval mode, default 200).
//
// stdout: session -> one JSON line {mode, wall_ms, counts};
//         eval -> line 1 {mode, wall_ms}, line 2 {eval_ms: [...]};
//         cold -> one JSON line {mode, wall_ms}.
// The harness (ladder.py) measures this process with wait4 for exact
// CPU/RSS. On any error: message on stderr, non-zero exit.

package main

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/chromedp/chromedp"
)

// Explicit IIFEs: chromedp sends the expression to Runtime.evaluate as-is,
// so a bare "() => ..." would come back as an uninvoked function object.
const (
	evalTitle     = `(() => document.title)()`
	evalCardCount = `(() => document.querySelectorAll('.card').length)()`
	evalExtract   = `(() => Array.from(document.querySelectorAll('.card')).map(c => ({t: c.querySelector('.t').textContent, p: c.querySelector('.p').textContent, v: c.dataset.vendor})))()`
	evalVisible   = `(() => document.querySelectorAll('.card:not(.hidden)').length)()`
	evalRows      = `(() => document.querySelectorAll('#rows tr').length)()`
	evalOne       = `(() => 1 + 1)()`
)

// Card mirrors the extract shape the correctness gate compares.
type Card struct {
	T string `json:"t"`
	P string `json:"p"`
	V string `json:"v"`
}

// newBrowser launches Chrome with the ladder's flags and returns a context
// for the first tab. The returned cancel func shuts the tab and the browser
// down (tab cancel, then allocator cancel).
func newBrowser(chromeBin string) (context.Context, context.CancelFunc) {
	opts := []chromedp.ExecAllocatorOption{
		chromedp.ExecPath(chromeBin),
		chromedp.Flag("headless", "new"),
		chromedp.Flag("no-sandbox", true),
		chromedp.Flag("disable-dev-shm-usage", true),
		chromedp.NoFirstRun,
		chromedp.NoDefaultBrowserCheck,
		chromedp.WindowSize(1440, 900),
	}
	allocCtx, allocCancel := chromedp.NewExecAllocator(context.Background(), opts...)
	ctx, cancel := chromedp.NewContext(allocCtx)
	return ctx, func() { cancel(); allocCancel() }
}

func runSession(base, chromeBin string) (map[string]any, error) {
	counts := map[string]any{}
	ctx, cancel := newBrowser(chromeBin)
	defer cancel()

	if _, err := chromedp.Run(ctx, chromedp.EmulateViewport(1440, 900)); err != nil {
		return nil, fmt.Errorf("viewport: %w", err)
	}
	if _, err := chromedp.Run(ctx, chromedp.Navigate(base+"/page_a.html")); err != nil {
		return nil, fmt.Errorf("goto A: %w", err)
	}
	titleA, err := chromedp.Run(ctx, chromedp.Evaluate[string](evalTitle))
	if err != nil {
		return nil, fmt.Errorf("title A: %w", err)
	}
	counts["title_a"] = titleA
	cardsA, err := chromedp.Run(ctx, chromedp.Evaluate[int64](evalCardCount))
	if err != nil {
		return nil, fmt.Errorf("card count: %w", err)
	}
	counts["cards_a"] = cardsA
	cards, err := chromedp.Run(ctx, chromedp.Evaluate[[]Card](evalExtract))
	if err != nil {
		return nil, fmt.Errorf("extract: %w", err)
	}
	raw, err := json.Marshal(cards)
	if err != nil {
		return nil, fmt.Errorf("marshal extract: %w", err)
	}
	if err := os.WriteFile(os.Getenv("LADDER_EXTRACT_OUT"), raw, 0o644); err != nil {
		return nil, fmt.Errorf("write extract: %w", err)
	}
	if _, err := chromedp.Run(ctx, chromedp.SendKeys(chromedp.CSS("#q"), "widget")); err != nil {
		return nil, fmt.Errorf("fill: %w", err)
	}
	if _, err := chromedp.Run(ctx, chromedp.Click(chromedp.CSS("#search"))); err != nil {
		return nil, fmt.Errorf("click: %w", err)
	}
	visible, err := chromedp.Run(ctx, chromedp.Evaluate[int64](evalVisible))
	if err != nil {
		return nil, fmt.Errorf("visible: %w", err)
	}
	counts["visible"] = visible
	shot, err := chromedp.Run(ctx, chromedp.CaptureScreenshot())
	if err != nil {
		return nil, fmt.Errorf("screenshot: %w", err)
	}
	if err := os.WriteFile(os.Getenv("LADDER_SHOT_OUT"), shot, 0o644); err != nil {
		return nil, fmt.Errorf("write shot: %w", err)
	}

	// New tab on the same browser.
	ctx2, cancel2 := chromedp.NewContext(ctx)
	defer cancel2()
	if _, err := chromedp.Run(ctx2, chromedp.EmulateViewport(1440, 900)); err != nil {
		return nil, fmt.Errorf("viewport B: %w", err)
	}
	if _, err := chromedp.Run(ctx2, chromedp.Navigate(base+"/page_b.html")); err != nil {
		return nil, fmt.Errorf("goto B: %w", err)
	}
	titleB, err := chromedp.Run(ctx2, chromedp.Evaluate[string](evalTitle))
	if err != nil {
		return nil, fmt.Errorf("title B: %w", err)
	}
	counts["title_b"] = titleB
	rowsB, err := chromedp.Run(ctx2, chromedp.Evaluate[int64](evalRows))
	if err != nil {
		return nil, fmt.Errorf("rows B: %w", err)
	}
	counts["rows_b"] = rowsB
	return counts, nil
}

func runEval(base, chromeBin string, n int) ([]float64, error) {
	ctx, cancel := newBrowser(chromeBin)
	defer cancel()
	if _, err := chromedp.Run(ctx, chromedp.Navigate(base+"/page_a.html")); err != nil {
		return nil, fmt.Errorf("goto A: %w", err)
	}
	lat := make([]float64, 0, n)
	for i := 0; i < n; i++ {
		t0 := time.Now()
		if _, err := chromedp.Run(ctx, chromedp.Evaluate[string](evalTitle)); err != nil {
			return nil, fmt.Errorf("eval %d: %w", i, err)
		}
		lat = append(lat, float64(time.Since(t0).Nanoseconds())/1e6)
	}
	return lat, nil
}

func runCold(chromeBin string) error {
	ctx, cancel := newBrowser(chromeBin)
	defer cancel()
	_, err := chromedp.Run(ctx, chromedp.Evaluate[int64](evalOne))
	return err
}

const (
	evalExampleTitle = `(() => document.title)()`
	evalExampleH1    = `(() => document.querySelector('h1')?.textContent ?? null)()`
	evalExamplePara  = `(() => document.querySelector('p')?.textContent?.trim().slice(0, 80) ?? null)()`
)

// waitForLoad polls document.readyState until 'complete' (chromedp's Navigate
// does not wait for page load by itself).
func waitForLoad(ctx context.Context) error {
	for i := 0; i < 150; i++ {
		ready, err := chromedp.Run(ctx, chromedp.Evaluate[string](`document.readyState`))
		if err == nil && ready == "complete" {
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return fmt.Errorf("waitForLoad: timeout")
}

func runRealworld(chromeBin string) (map[string]any, error) {
	ctx, cancel := newBrowser(chromeBin)
	defer cancel()
	if _, err := chromedp.Run(ctx, chromedp.EmulateViewport(1440, 900)); err != nil {
		return nil, fmt.Errorf("viewport: %w", err)
	}
	ops := map[string]any{}
	t := time.Now()
	if _, err := chromedp.Run(ctx, chromedp.Navigate("https://example.com")); err != nil {
		return nil, fmt.Errorf("goto: %w", err)
	}
	if err := waitForLoad(ctx); err != nil {
		return nil, fmt.Errorf("load: %w", err)
	}
	ops["goto_ms"] = float64(time.Since(t).Nanoseconds()) / 1e6
	t = time.Now()
	title, err := chromedp.Run(ctx, chromedp.Evaluate[*string](evalExampleTitle))
	if err != nil {
		return nil, fmt.Errorf("title: %w", err)
	}
	ops["title_ms"] = float64(time.Since(t).Nanoseconds()) / 1e6
	h1, err := chromedp.Run(ctx, chromedp.Evaluate[*string](evalExampleH1))
	if err != nil {
		return nil, fmt.Errorf("h1: %w", err)
	}
	para, err := chromedp.Run(ctx, chromedp.Evaluate[*string](evalExamplePara))
	if err != nil {
		return nil, fmt.Errorf("para: %w", err)
	}
	return map[string]any{"title": title, "h1": h1, "para": para, "ops": ops}, nil
}

func runBrowse(chromeBin string) (map[string]any, error) {
	ctx, cancel := newBrowser(chromeBin)
	defer cancel()
	if _, err := chromedp.Run(ctx, chromedp.EmulateViewport(1440, 900)); err != nil {
		return nil, fmt.Errorf("viewport: %w", err)
	}
	facts := map[string]any{}
	ops := map[string]any{}
	// awesome list
	t := time.Now()
	if _, err := chromedp.Run(ctx, chromedp.Navigate("https://github.com/sindresorhus/awesome")); err != nil {
		return nil, fmt.Errorf("goto awesome: %w", err)
	}
	if err := waitForLoad(ctx); err != nil {
		return nil, fmt.Errorf("load awesome: %w", err)
	}
	ops["awesome_goto_ms"] = float64(time.Since(t).Nanoseconds()) / 1e6
	awesomeTitle, err := chromedp.Run(ctx, chromedp.Evaluate[*string](`(() => document.title ?? null)()`))
	if err != nil {
		return nil, fmt.Errorf("awesome title: %w", err)
	}
	facts["awesome_title"] = awesomeTitle
	links, err := chromedp.Run(ctx, chromedp.Evaluate[int64](`(() => document.querySelectorAll('a').length)()`))
	if err != nil {
		return nil, fmt.Errorf("awesome links: %w", err)
	}
	facts["awesome_links"] = links
	scrollYs := make([]float64, 0, 5)
	for i := 0; i < 5; i++ {
		t := time.Now()
		y, err := chromedp.Run(ctx, chromedp.Evaluate[float64](`(() => { window.scrollBy({ top: 800, behavior: 'instant' }); return window.scrollY; })()`))
		if err != nil {
			return nil, fmt.Errorf("scroll %d: %w", i, err)
		}
		scrollYs = append(scrollYs, y)
		ops[fmt.Sprintf("awesome_scroll%d_ms", i)] = float64(time.Since(t).Nanoseconds()) / 1e6
		time.Sleep(300 * time.Millisecond)
	}
	facts["awesome_scroll_ys"] = scrollYs
	headings, err := chromedp.Run(ctx, chromedp.Evaluate[[]*string](`(() => [...document.querySelectorAll('h2')].slice(0, 5).map(h => h.textContent?.trim() ?? null))()`))
	if err != nil {
		return nil, fmt.Errorf("headings: %w", err)
	}
	facts["awesome_headings"] = headings
	// trending
	t = time.Now()
	if _, err := chromedp.Run(ctx, chromedp.Navigate("https://github.com/trending")); err != nil {
		return nil, fmt.Errorf("goto trending: %w", err)
	}
	if err := waitForLoad(ctx); err != nil {
		return nil, fmt.Errorf("load trending: %w", err)
	}
	ops["trending_goto_ms"] = float64(time.Since(t).Nanoseconds()) / 1e6
	trendingTitle, err := chromedp.Run(ctx, chromedp.Evaluate[*string](`(() => document.title ?? null)()`))
	if err != nil {
		return nil, fmt.Errorf("trending title: %w", err)
	}
	facts["trending_title"] = trendingTitle
	repos, err := chromedp.Run(ctx, chromedp.Evaluate[int64](`(() => document.querySelectorAll('article.Box-row').length)()`))
	if err != nil {
		return nil, fmt.Errorf("trending repos: %w", err)
	}
	facts["trending_repos"] = repos
	top3, err := chromedp.Run(ctx, chromedp.Evaluate[[]string](`(() => [...document.querySelectorAll('article.Box-row h2 a')].slice(0, 3).map(a => (a.textContent ?? '').replace(/\s+/g, '')))()`))
	if err != nil {
		return nil, fmt.Errorf("top3: %w", err)
	}
	facts["trending_top3"] = top3
	t = time.Now()
	if _, err := chromedp.Run(ctx, chromedp.Click(chromedp.CSS(`article.Box-row h2 a`))); err != nil {
		return nil, fmt.Errorf("click: %w", err)
	}
	var newURL *string
	for i := 0; i < 24; i++ {
		time.Sleep(250 * time.Millisecond)
		u, err := chromedp.Run(ctx, chromedp.Evaluate[string](`(() => location.href)()`))
		if err != nil {
			continue
		}
		if !strings.Contains(u, "trending") {
			newURL = &u
			break
		}
	}
	ops["trending_click_ms"] = float64(time.Since(t).Nanoseconds()) / 1e6
	facts["trending_clicked_url"] = newURL
	clickedTitle, err := chromedp.Run(ctx, chromedp.Evaluate[*string](`(() => document.title ?? null)()`))
	if err != nil {
		return nil, fmt.Errorf("clicked title: %w", err)
	}
	facts["trending_clicked_title"] = clickedTitle
	return map[string]any{"facts": facts, "ops": ops}, nil
}

func emit(v any) {
	raw, err := json.Marshal(v)
	if err != nil {
		fmt.Fprintf(os.Stderr, "[chromedp] fatal: marshal: %v\n", err)
		os.Exit(1)
	}
	fmt.Println(string(raw))
}

func main() {
	if err := realMain(); err != nil {
		fmt.Fprintf(os.Stderr, "[chromedp] fatal: %v\n", err)
		os.Exit(1)
	}
}

func realMain() error {
	mode := "session"
	if len(os.Args) > 1 {
		mode = os.Args[1]
	}
	base := os.Getenv("LADDER_BASE")
	chromeBin := os.Getenv("CHROME_BIN")
	t0 := time.Now()
	wallMs := func() float64 { return float64(time.Since(t0).Nanoseconds()) / 1e6 }

	switch mode {
	case "session":
		counts, err := runSession(base, chromeBin)
		if err != nil {
			return err
		}
		emit(map[string]any{"mode": "session", "wall_ms": wallMs(), "counts": counts})
	case "eval":
		n := 200
		if s := os.Getenv("LADDER_N_EVAL"); s != "" {
			if v, err := strconv.Atoi(s); err == nil {
				n = v
			}
		}
		lat, err := runEval(base, chromeBin, n)
		if err != nil {
			return err
		}
		emit(map[string]any{"mode": "eval", "wall_ms": wallMs()})
		emit(map[string]any{"eval_ms": lat})
	case "cold":
		if err := runCold(chromeBin); err != nil {
			return err
		}
		emit(map[string]any{"mode": "cold", "wall_ms": wallMs()})
	case "realworld":
		rw, err := runRealworld(chromeBin)
		if err != nil {
			return err
		}
		emit(map[string]any{"mode": "realworld", "wall_ms": wallMs(), "realworld": rw})
	case "browse":
		br, err := runBrowse(chromeBin)
		if err != nil {
			return err
		}
		emit(map[string]any{"mode": "browse", "wall_ms": wallMs(), "realworld": br})
	default:
		return fmt.Errorf("unknown mode %q", mode)
	}
	return nil
}
