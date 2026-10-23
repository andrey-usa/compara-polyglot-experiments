# primavera bench

primavera (`../primavera`, linked against `../navigera`) against Playwright
Test on the same 7 scenarios (`../primavera/examples/bench-site` and
`pw/tests`), against navigera's bench site, on one runner with the same
Chrome. `bench.py` interleaves rounds and reports suite wall time (cold
first round and median), one scenario from a cold start, and peak memory of
the runner's own processes and of everything including Chrome. Workflow:
`.github/workflows/primavera-bench.yml`.
