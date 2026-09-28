Name:           xearthlayer
Version:        0.5.0
Release:        1%{?dist}
Summary:        High-quality satellite imagery for X-Plane, streamed on demand

License:        MIT
URL:            https://github.com/samsoir/xearthlayer
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  rust >= 1.70
BuildRequires:  cargo
BuildRequires:  fuse3-devel

Requires:       fuse3

# Disable debug packages (release builds have no debug symbols)
%global debug_package %{nil}

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

# Publishing tools are a separate package (#284). They need neither FUSE nor
# X-Plane, and a flight simulator user installing xearthlayer should not pull
# in a map renderer and a second TLS stack to build packages they never will.
%package        publish
Summary:        Create, build and release XEarthLayer scenery packages

%description    publish
xearthlayer-publish creates distributable XEarthLayer scenery packages from
Ortho4XP output: repository management, archive building and splitting,
versioning, release to a package library, coverage maps and zoom level
dedupe. It shares the package format with xearthlayer and nothing else.

%prep
%autosetup

%build
cargo build --release --locked

%install
install -Dm755 target/release/xearthlayer %{buildroot}%{_bindir}/xearthlayer
install -Dm755 target/release/xearthlayer-publish %{buildroot}%{_bindir}/xearthlayer-publish
install -Dm644 LICENSE %{buildroot}%{_licensedir}/%{name}/LICENSE
install -Dm644 README.md %{buildroot}%{_docdir}/%{name}/README.md

%files
%license LICENSE
%doc README.md
%{_bindir}/xearthlayer

%files publish
%license LICENSE
%doc docs/content-publishing.md
%{_bindir}/xearthlayer-publish

%changelog
* Mon Dec 15 2025 Sam de Freyssinet <sam@def.reyssi.net> - 0.2.0-1
- Initial RPM release
- Async pipeline architecture for improved performance
- HTTP concurrency limiting to prevent network exhaustion
- Cooperative cancellation for FUSE timeout handling
- TUI dashboard for real-time monitoring
