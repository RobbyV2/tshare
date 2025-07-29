# TShare

[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](https://opensource.org/licenses/MIT)
[![Rust](https://img.shields.io/badge/rust-1.88-blue.svg)](https://www.rust-lang.org)
[![GitHub](https://img.shields.io/github/stars/RobbyV2/tshare?style=social)](https://github.com/RobbyV2/tshare)

[![Version](https://img.shields.io/badge/Version-1.0.3-blue.svg)](https://www.rust-lang.org)

Collaborative terminal sharing.

## Screenshots

<div align="center">
  <img src="resources/1.png" alt="TShare Screenshot 1" width="45%" />
  <img src="resources/2.png" alt="TShare Screenshot 2" width="45%" />
  <br><br>
  <img src="resources/3.png" alt="TShare Screenshot 3" width="90%" />
</div>

## Requirements

- Rust (any recent version)

Development dependencies:
- just (`cargo install just`)
- djlint (`pip install djlint`) for HTML formatting

## Installation

Download pre-built packages from the [releases page](https://github.com/RobbyV2/tshare/releases) or build from source:

```bash
cargo build --release
# Binaries will be in target/release/
```

## Usage

Start servers:
```bash
tshare tunnel &
tshare web &
```

Share terminal:
```bash
tshare connect
```

## Development

See `justfile` for available commands:
```bash
just --list
```

Common commands:
```bash
just run            # Start both servers
just client connect # Create session
just build          # Build release
just build-deb      # Build .deb package
```

## Architecture

- `tshare`: CLI client, captures terminal sessions
- `tshare tunnel`: WebSocket relay and API, port 8385
- `tshare web`: Web interface, port 8386

## Configuration

All binaries accept `--help` for options. Default configuration works for local development.

Production example:
```bash
tshare tunnel --host 0.0.0.0
tshare web --host 0.0.0.0 --tunnel-url http://tunnel.example.com:8385
tshare connect --tunnel-host tunnel.example.com --web-host web.example.com
```

## License

MIT