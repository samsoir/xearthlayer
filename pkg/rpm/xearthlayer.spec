Name:           xearthlayer
Version:        0.2.0
Release:        1%{?dist}
Summary:        High-quality satellite imagery for X-Plane, streamed on demand

License:        MIT
URL:            https://github.com/samsoir/xearthlayer
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  rust >= 1.70
BuildRequires:  cargo
BuildRequires:  fuse3-devel

Requires:       fuse3

%description
XEarthLayer delivers satellite/aerial imagery to X-Plane without massive
downloads. Instead of pre-downloading thousands of gigabytes of textures,
XEarthLayer installs small regional packages and streams textures on-demand
as you fly.

Features:
- Small regional packages (megabytes, not gigabytes)
- On-demand texture streaming from Bing Maps or Google Maps
- Two-tier caching for instant repeat visits
- High-quality BC1/BC3 DDS textures with mipmaps
- Works with Ortho4XP-generated scenery
- Linux support (Windows and macOS planned)

%prep
%autosetup

%build
cargo build --release --locked

%install
install -Dm755 target/release/xearthlayer %{buildroot}%{_bindir}/xearthlayer
install -Dm644 LICENSE %{buildroot}%{_licensedir}/%{name}/LICENSE
install -Dm644 README.md %{buildroot}%{_docdir}/%{name}/README.md

%files
%license LICENSE
%doc README.md
%{_bindir}/xearthlayer

%changelog
* Sun Dec 15 2025 Sam de Freyssinet <sam@def.reyssi.net> - 0.2.0-1
- Initial RPM release
- Async pipeline architecture for improved performance
- HTTP concurrency limiting to prevent network exhaustion
- Cooperative cancellation for FUSE timeout handling
- TUI dashboard for real-time monitoring
