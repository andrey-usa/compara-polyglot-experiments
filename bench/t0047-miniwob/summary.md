# T-0047 MiniWoB cell - navigera with clickable refs (strict scoring)

- recovered from artifact `miniwob-eval-t0047-axclick-o0c112s3` of CI run 40401302109 by publish-t0047 run 40415972902
- branch commit: e5f0ab3cee793b72fffde2d35a09b81873cd512b
- T-0047 cell 1 (navigera ax-clickable, 112 tasks x 3 seeds, DeepSeek-V4.1-Flash): the eval step completed and the artifact was uploaded, but the eval job's own publish step failed, so the numbers are recovered here from the artifact
- episodes file: `miniwob-episodes-navigera-20261026T210624Z-3285.jsonl`
- scoring: solved = `done` and `raw_reward == 1`

**298/336 strict solves (88.7%)**; computed spend $1.012583; 5778905 tokens; 9 episodes with an error; episodes using `eval` to read the DOM: 78; episodes clicking a `clickable` ref: 39.

## Per task (seeds solved out of 3)

| task | solved |
|---|---|
| book-flight | 1/3 |
| book-flight-nodelay | 1/3 |
| button-delay | 3/3 |
| buy-ticket | 3/3 |
| choose-date | 2/3 |
| choose-date-easy | 3/3 |
| choose-date-medium | 3/3 |
| choose-date-nodelay | 3/3 |
| choose-list | 3/3 |
| click-button | 3/3 |
| click-button-sequence | 3/3 |
| click-checkboxes | 3/3 |
| click-checkboxes-large | 3/3 |
| click-checkboxes-soft | 3/3 |
| click-checkboxes-transfer | 3/3 |
| click-collapsible | 3/3 |
| click-collapsible-2 | 3/3 |
| click-collapsible-2-nodelay | 3/3 |
| click-collapsible-nodelay | 3/3 |
| click-color | 2/3 |
| click-dialog | 3/3 |
| click-dialog-2 | 3/3 |
| click-link | 3/3 |
| click-menu | 2/3 |
| click-menu-2 | 3/3 |
| click-option | 3/3 |
| click-pie | 3/3 |
| click-pie-nodelay | 3/3 |
| click-scroll-list | 3/3 |
| click-shades | 3/3 |
| click-tab | 3/3 |
| click-tab-2 | 3/3 |
| click-tab-2-easy | 3/3 |
| click-tab-2-hard | 3/3 |
| click-tab-2-medium | 3/3 |
| click-test | 3/3 |
| click-test-2 | 3/3 |
| click-test-transfer | 3/3 |
| click-widget | 3/3 |
| copy-paste | 1/3 |
| copy-paste-2 | 0/3 |
| daily-calendar | 1/3 |
| drag-box | 3/3 |
| drag-cube | 0/3 |
| drag-items | 3/3 |
| drag-items-grid | 1/3 |
| drag-sort-numbers | 3/3 |
| draw-circle | 0/3 |
| draw-line | 0/3 |
| email-inbox | 3/3 |
| email-inbox-delete | 3/3 |
| email-inbox-forward | 3/3 |
| email-inbox-forward-nl | 3/3 |
| email-inbox-forward-nl-turk | 3/3 |
| email-inbox-important | 3/3 |
| email-inbox-nl-turk | 2/3 |
| email-inbox-noscroll | 3/3 |
| email-inbox-reply | 3/3 |
| email-inbox-star-reply | 3/3 |
| enter-date | 3/3 |
| enter-password | 3/3 |
| enter-text | 3/3 |
| enter-text-2 | 3/3 |
| enter-text-dynamic | 3/3 |
| enter-time | 3/3 |
| find-greatest | 3/3 |
| find-word | 3/3 |
| focus-text | 3/3 |
| focus-text-2 | 3/3 |
| form-sequence | 2/3 |
| form-sequence-2 | 3/3 |
| form-sequence-3 | 3/3 |
| generate-number | 3/3 |
| guess-number | 3/3 |
| highlight-text | 3/3 |
| highlight-text-2 | 3/3 |
| hot-cold | 0/3 |
| login-user | 3/3 |
| login-user-popup | 3/3 |
| multi-layouts | 3/3 |
| multi-orderings | 3/3 |
| navigate-tree | 3/3 |
| number-checkboxes | 0/3 |
| odd-or-even | 3/3 |
| order-food | 3/3 |
| phone-book | 3/3 |
| read-table | 3/3 |
| read-table-2 | 3/3 |
| resize-textarea | 3/3 |
| scroll-text | 3/3 |
| scroll-text-2 | 3/3 |
| search-engine | 3/3 |
| sign-agreement | 3/3 |
| simon-says | 0/3 |
| simple-algebra | 3/3 |
| simple-arithmetic | 3/3 |
| social-media | 3/3 |
| social-media-all | 3/3 |
| social-media-some | 3/3 |
| terminal | 3/3 |
| text-editor | 3/3 |
| text-transform | 3/3 |
| tic-tac-toe | 2/3 |
| unicode-test | 3/3 |
| use-autocomplete | 3/3 |
| use-autocomplete-nodelay | 3/3 |
| use-colorwheel | 3/3 |
| use-colorwheel-2 | 2/3 |
| use-slider | 3/3 |
| use-slider-2 | 3/3 |
| use-spinner | 3/3 |
| visual-addition | 3/3 |

