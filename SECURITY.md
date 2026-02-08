# Security Policy

## Supported Versions

| Version | Supported |
|---------|-----------|
| Latest release | Yes |
| Older releases | No |

Only the latest release receives security updates. We recommend always running the most recent version.

## Reporting a Vulnerability

**Do not open a public issue for security vulnerabilities.**

Instead, please report vulnerabilities privately:

1. **GitHub Security Advisories** (preferred): Use [Report a vulnerability](https://github.com/samsoir/xearthlayer/security/advisories/new) to submit a private report directly on GitHub.
2. **Email**: Contact the maintainers directly if the advisory feature is unavailable.

### What to Include

- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (if you have one)

### What to Expect

- **Acknowledgment** within 48 hours
- **Assessment** within 1 week
- **Fix timeline** communicated once the severity is assessed
- **Credit** in the release notes (unless you prefer to remain anonymous)

## Scope

XEarthLayer runs as a userspace FUSE filesystem and makes HTTP requests to satellite imagery providers. Areas of particular security concern include:

- **FUSE filesystem operations** — path traversal, symlink handling
- **Network requests** — URL construction, response validation
- **Configuration parsing** — INI file handling, path expansion (`~`)
- **Cache management** — file creation, directory traversal
- **Dependency vulnerabilities** — outdated or compromised crates

## Security Practices

- Dependencies are monitored with `cargo audit`
- All inputs from configuration files and network responses are validated
- FUSE operations validate paths to prevent directory traversal
- No secrets are stored in the repository
