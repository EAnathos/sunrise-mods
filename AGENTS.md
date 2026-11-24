# Registry v2 instructions

- Follow [REVIEWING.md](REVIEWING.md): release reviews look only for malware, dangerous patterns or suspicious obfuscation. The approving reviewer is responsible.
- Do not police bugs, style, copyright, licences or provenance. Send ordinary bugs or suggestions to the author's repository without blocking the release.
- Format, hashes, signatures and builds are automatic bot checks.
- Treat all author- and PR-controlled input as hostile; it must never cause a panic. Never interpolate it into `run:` using `${{ }}`; pass it as data instead.
- Keep permissions minimal per job. Never run author code with a write token, `id-token` access or secrets.
- Pin actions by full commit SHA with a version comment. Use `persist-credentials: false` on checkout.
- Escape `::`, `##[` and newlines in untrusted log and summary text.
- See [docs/registry-v2.md](docs/registry-v2.md) for the v2 reference.
