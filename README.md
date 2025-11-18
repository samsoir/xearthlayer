# XEarthLayer

A high-performance Rust implementation for streaming satellite imagery to X-Plane flight simulator.

## Overview

XEarthLayer provides on-demand satellite imagery streaming for X-Plane using a FUSE virtual filesystem. Instead of downloading massive scenery packages, it dynamically fetches only the imagery you need as you fly, from multiple satellite providers (Bing Maps, NAIP, EOX Sentinel-2, USGS).

## Getting Started

```bash
# Initialize development environment
make init

# Run tests
make test

# Build and run
make run
```

See `make help` for all available commands.

## Credits

This project is architecturally influenced by [AutoOrtho](https://github.com/kubilus1/autoortho) by [kubilus1](https://github.com/kubilus1). XEarthLayer is an independent Rust implementation focused on performance, memory safety, and cross-platform compatibility.

## License

Licensed under the MIT License. See [LICENSE](LICENSE) for details.
