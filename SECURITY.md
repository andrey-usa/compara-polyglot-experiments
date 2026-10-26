# Security

primavera runs what a test file says: browser steps, `exec` of programs,
`http` calls, a `[server]` command. Run test files you trust, as you would
a script.

## Reporting a vulnerability

Report it privately through GitHub: the repository's **Security** tab,
**Report a vulnerability**. Please don't open a public issue. You'll get
an answer within a few days; a fix and an advisory follow for a confirmed
issue.

## What counts

- A secret given as `${secret:…}` reaching any output: the console, JUnit,
  JSON lines, the HTML report, a trace, a screenshot name, a log.
- A page making primavera (or the navigera in it) read or write a file,
  run a program, or reach the network beyond what the test file asks.
- `primavera mcp` doing for a client more than its tools describe.
- The GitHub Action, the release workflow or the container image running
  code they shouldn't, or shipping something other than what was built.
