# Security Policy

## Reporting a vulnerability

Please report security vulnerabilities privately through GitHub Security Advisories:

**https://github.com/tickloopai/tickloop/security/advisories/new**

Do not open a public issue, pull request or discussion for a suspected vulnerability. Include what you can of the following:

- the affected version (`tickloop --version`) and installation method;
- a description of the issue and its impact;
- steps to reproduce, a proof of concept, or the relevant code location;
- any suggested fix or mitigation.

We will acknowledge the report, keep you informed while we investigate, and credit you in the advisory unless you prefer otherwise. Please give us a reasonable time to release a fix before disclosing the issue publicly.

## Supported versions

Only the latest release receives security fixes. Upgrade with `tickloop update` or through your installation method before reporting.

## Scope

Tickloop runs locally with your user's privileges, so the following areas are especially relevant:

- **Command and tool execution.** The agent runs shell commands, edits files and calls other tools under a permission system (Auto, Ask, Plan and Full Access modes, permission rules, working-directory trust, single-use approvals). Ways to make the agent run an action that the active mode and rules should have stopped, including through prompt injection in tool output, files or web content, are in scope.
- **Local credentials.** Tickloop stores model-provider API keys and OAuth tokens, Tickloop Accounts credentials, and Tick package credentials (API keys and OAuth tokens for the third-party accounts that Tick packages access) in its configuration root (`~/.tickloop` by default). Leaks of these credentials into transcripts, approvals, tool results, logs, other processes or the network are in scope.
- **Tickhost and Ticks.** Tickhost starts local programs that the agent opens as Ticks and connects them to the agent. Problems with program resolution, the scope of the credentials passed to a package's process, or the isolation between Tick instances, views and agents are in scope.
- **Local daemon.** The agent daemon and Tickhost listen on private Unix sockets. Access by other local users is in scope.
- **Installation and updates.** The install script, the Cargo launcher and the updater download and verify release archives. Bypassing their verification is in scope.

Out of scope:

- actions you explicitly approved, or actions allowed by Full Access mode or by rules you configured;
- vulnerabilities in model providers, the services Tick packages access, Herdr or other third-party services and programs (report those upstream);
- Tick packages from [tickloopai/ticks](https://github.com/tickloopai/ticks); report those in that repository's security advisories.

The working directory is not an operating-system sandbox. Tickloop's permission checks are not a substitute for running untrusted code in an isolated environment.
