# suite40

primavera's 40-scenario suite (`pv/`) and its Playwright Test twin (`pw/`),
on two local sites: a shop (`site/server.py`, port 18001) and the gauntlet
of awkward pages (`gauntlet/server.py`, ports 18201/18202). CI runs it on
three OSes (`.github/workflows/ci.yml`, "The 40-scenario suite"):

```sh
python3 site/server.py --port 18001 &
python3 gauntlet/server.py --port 18201 --xo-port 18202 --seed 7 --jitter 0 --telemetry 1 &
GAUNTLET_URL=http://127.0.0.1:18201 primavera run pv --base-url http://127.0.0.1:18001 --workers 4
```
