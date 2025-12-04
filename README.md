# XEarthLayer

High-quality satellite imagery for X-Plane, streamed on demand.

## What It Does

XEarthLayer delivers satellite/aerial imagery to X-Plane without massive downloads. Instead of pre-downloading hundreds of gigabytes of textures, XEarthLayer:

1. **Installs small regional packages** (megabytes) containing terrain definitions
2. **Streams textures on-demand** as you fly, generating them from satellite imagery providers

The result: complete orthophoto scenery with minimal disk usage and no lengthy initial downloads.

## How It Works

```
Regional Package (small)          XEarthLayer Service (running)
┌────────────────────────┐        ┌────────────────────────┐
│ Terrain definitions    │        │ Satellite Providers    │
│ (DSF, TER files)       │───────→│ (Bing, Google)         │
│ References textures    │        │                        │
│ that don't exist       │        │ Generates DDS textures │
└────────────────────────┘        │ on-demand              │
                                  └────────────────────────┘
                                             │
                                             ▼
                                  ┌────────────────────────┐
                                  │ X-Plane sees complete  │
                                  │ scenery with textures  │
                                  └────────────────────────┘
```

See [How It Works](docs/how-it-works.md) for detailed architecture.

## Features

- Small regional packages (megabytes, not gigabytes)
- On-demand texture streaming from Bing Maps or Google Maps
- Two-tier caching for instant repeat visits
- High-quality BC1/BC3 DDS textures with mipmaps
- Works with Ortho4XP-generated scenery
- Linux support (Windows and macOS planned)

## Quick Start

```bash
# Build from source
git clone https://github.com/youruser/xearthlayer.git
cd xearthlayer
make release

# Initialize configuration
xearthlayer init

# Configure your package library in ~/.xearthlayer/config.ini:
# [packages]
# library_url = https://example.com/xearthlayer_package_library.txt

# Install a regional package
xearthlayer packages install eu-paris

# Start the streaming service
xearthlayer start --source "Custom Scenery/zzXEL_eu-paris_ortho"

# Add the _xel mount point to X-Plane's scenery_packs.ini
# Fly!
```

See [Getting Started](docs/getting-started.md) for the complete guide.

## Documentation

### User Guides

| Guide | Description |
|-------|-------------|
| [How It Works](docs/how-it-works.md) | Architecture and system overview |
| [Getting Started](docs/getting-started.md) | First-time setup and usage |
| [Configuration](docs/configuration.md) | All configuration options |
| [Package Management](docs/package-management.md) | Installing, updating, removing packages |
| [Running the Service](docs/running-service.md) | Streaming service options |
| [Content Publishing](docs/content-publishing.md) | Create packages from Ortho4XP |

### Developer Documentation

See [Developer Documentation](docs/dev/) for architecture, design principles, and implementation details.

## CLI Reference

```bash
# Setup
xearthlayer init                      # Create config file

# Package Management
xearthlayer packages check            # Check available packages
xearthlayer packages install <region> # Install a package
xearthlayer packages list             # List installed packages
xearthlayer packages update [region]  # Update packages
xearthlayer packages remove <region>  # Remove a package

# Streaming Service
xearthlayer start --source <path>     # Start streaming (mount on <path>_xel)
xearthlayer cache stats               # View cache usage
xearthlayer cache clear               # Clear cache

# Content Publishing
xearthlayer publish init              # Initialize repository
xearthlayer publish add --source <path> --region <code>  # Create package
xearthlayer publish build --region <code>   # Build archives
xearthlayer publish release --region <code> # Release to library
```

Run `xearthlayer --help` for all options.

## Requirements

- **X-Plane 12** (X-Plane 11 may work but is untested)
- **Linux** with FUSE support
- **Internet connection** for streaming imagery

## Contributing

```bash
# Install Rust via rustup.rs
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Clone and build
git clone https://github.com/youruser/xearthlayer.git
cd xearthlayer
make init
make verify
```

See [Developer Documentation](docs/dev/) for architecture and guidelines.

## Credits

Architecturally influenced by [AutoOrtho](https://github.com/kubilus1/autoortho) by [kubilus1](https://github.com/kubilus1). XEarthLayer is an independent Rust implementation focused on performance and memory safety.

Developed with assistance from [Claude](https://claude.ai) by Anthropic.

## License

Licensed under the MIT License. See [LICENSE](LICENSE) for details.
