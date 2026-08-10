# Security policy

**English** | [简体中文](SECURITY.zh-CN.md)

## Supported versions

liteavd is pre-alpha. Security fixes target the latest published 0.1.x
prerelease and the current `master` branch. Older snapshots are not supported.

## Reporting a vulnerability

Use GitHub's **Report a vulnerability** / private vulnerability reporting entry
for `ydog12138/liteavd`. Do not disclose the issue in a public GitHub issue,
discussion, log paste, or social channel before a fix is available.

Include, when relevant:

- affected commit/tag and Flatpak version;
- host distribution, architecture, and sandbox status;
- Android Emulator/system-image versions;
- exact managed/recovered/adopted session origin;
- reproduction steps and expected/actual security boundary;
- whether credentials, host files, guest data, ports, or microphone data were
  exposed;
- a minimal proof of concept with secrets removed.

The maintainer will acknowledge a valid report through GitHub, assess severity,
coordinate a fix and disclosure, and credit the reporter if requested.

## Security boundaries worth reporting

- managed gRPC reachable without the session JWT or beyond loopback;
- cross-session or reused-port input/operation delivery;
- stopping or signaling an unrelated process;
- Flatpak access outside declared permissions or exact portal grants;
- archive traversal, component overwrite, or license bypass;
- persistent host microphone capture after the UI indicates stop;
- leakage of JWT private keys, guest PCM, or sensitive session data;
- unbounded attacker-controlled memory/disk/process growth.

An upstream Android Emulator crash without a liteavd boundary violation may be
tracked as a normal bug, but please report privately first if it exposes host or
guest data.

## Defensive design summary

Managed sessions use per-session ES256/JWT credentials and a minimum allowlist.
Operations freeze exact session routes. Process termination verifies executable
identity and ports. Downloads verify hashes and archives reject traversal.
Private files use restrictive modes and atomic publication. The Flatpak avoids
broad filesystem grants, and microphone capture is explicit and non-persistent.
