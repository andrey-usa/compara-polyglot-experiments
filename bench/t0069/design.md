# T-0069 design draft (local, pre-execution)

## Site
bench/suite40/site/server.py @ primavera next 76d22b9 (Acme Supply, stdlib,
in-memory state, GET /__state for verification). Local copy with changes ->
experiment-38 (allocated via ens alloc-branch), never in primavera.

## 8 write tasks (from suite40 shop-scenario names + intent; agents never see .pv)
W1 search: search "wrench" via Enter; results list contains wrench products.
W2 sort-price: sort by price ascending; first 3 prices non-decreasing.
W3 add-grid: add a grid product to cart; cart count = 1; /__state cart has it.
W4 cart-math: add two products; cart page subtotal/shipping/total consistent.
W5 coupon-valid: apply valid coupon; discount line appears, total reduced.
W6 signin: log in with valid creds; /account shows the seeded orders.
W7 support-ticket: submit support form (iframe: radios+select+upload); ticket in /__state.
W8 checkout-order: complete checkout; order recorded in /__state.
Verify via /__state where possible (what the agent DID, not what it claims).

## 4 repair tasks (passing test, then site change; broken-feature switch each)
Validated 2026-10-25: both arms' ref tests pass pristine, fail on changed,
and an adapted path succeeds on every changed site.
R1 button renamed: grid buy button "Add to cart" -> "Add to basket"
   (accessible name changes).
R2 card-as-button: product grid card loses its <a>; an "Open" button with a
   generic accessible name opens the detail (name-based link lookup fails;
   agent must map the adjacent h3 name to the card's generic control).
R3 modal overlay: cookie banner becomes a modal covering every page
   (re-appears on each navigation; must dismiss per page).
R4 shadow search: search input moves into <acme-search> (open shadow root);
   its <label for> becomes a <span> (label association lost).
R4 note (2026-10-26): the R4 seed ref test asserts weakly (status contains
   "products" + a link that is visible even unfiltered), so it passes even
   with broken=search. Scoring still requires fail-when-broken, and the
   repair prompt states it - a repair that only fixes the locator scores 0.
   Validated achievable: asserting the narrowed count ("1 of 1") scores 1
   on both arms.
Switch: --change rN serves the changed site; the 8 broken switches
(--broken search/sort/add-cart/detail/coupon/signin/tickets/checkout, one per
write-task feature) make the underlying feature silently fail; a test that
passes both ways scores as fail.

## Arms (same model/harness loop as T-0022/T-0046; fresh context; shell only; 30 turns)
A: primavera next 76d22b9, instructions = `primavera skill` output.
B: Playwright Test current 1.x + Chromium, instructions = same-length skill
   drafted from Playwright docs (kept in the report).
3 trials per task per arm. Score: pass-on-site AND fail-when-broken, turns,
in/out tokens, cost (WAL), wall.
