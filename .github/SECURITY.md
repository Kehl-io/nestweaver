# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in NestWeaver, please report it
responsibly.

**Do NOT open a public GitHub issue for security vulnerabilities.**

Instead, please email **kory@kehl.io** with:

- A description of the vulnerability
- Steps to reproduce
- Potential impact
- Any suggested fixes (optional)

You should receive a response within 72 hours. Once the issue is confirmed,
a fix will be developed and released as a patch version before public
disclosure.

## Scope

Security issues include but are not limited to:

- Cypher/query injection via crafted symbol names or file paths
- Path traversal allowing file access outside the workspace sandbox
- Command injection via git operations
- Snapshot tampering bypassing integrity checks
- Credential leakage through config files or logs

## Supported Versions

| Version | Supported |
|---------|-----------|
| 0.1.x   | Yes       |
