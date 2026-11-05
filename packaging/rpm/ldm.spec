# Binary RPM of LDM, built from the same staged /usr tree as the .deb (packaging/stage-system.sh)
# by packaging/build-rpm.sh; nothing is compiled here. Defines passed by build-rpm.sh:
#   ldm_version  version from Cargo.toml        ldm_stage  staged install tree
%global debug_package %{nil}
%global _build_id_links none
# the binaries are already stripped release builds; the GUI is plain Python run from /usr/lib/ldm
%global __os_install_post %{nil}
%global __requires_exclude ^python\\(abi\\)
%define _binary_payload w19.zstdio

Name:           ldm
Version:        %{ldm_version}
Release:        1
Summary:        Linux download manager with browser video sniffing
License:        MIT
URL:            https://github.com/wzk4work/ldm
# GUI (ldm-gui, ldm-dbus); the daemon and CLI work without them
Recommends:     python3-pyside6
# HLS/DASH muxing (Fedora: ffmpeg-free or RPM Fusion ffmpeg; openSUSE: ffmpeg-7 or Packman)
Recommends:     /usr/bin/ffmpeg
# signature check of the LDM-managed yt-dlp download
Suggests:       /usr/bin/gpg

%description
Multi-connection HTTP(S)/FTP downloads with resume, HLS/DASH capture,
native-messaging browser integration (Chromium and Firefox), CLI, daemon
(systemd user unit ldmd.service) and a Qt (PySide6) GUI.

yt-dlp and deno are not shipped: on first use (with consent) LDM downloads
the official GitHub releases into each user's data dir
(~/.local/share/ldm/tools), verified by SHA-256 (and GPG for yt-dlp when
gpg is installed); see `ldm tools`.

%install
cp -a %{ldm_stage}/. %{buildroot}/
install -d %{buildroot}%{_defaultlicensedir}/%{name}
mv %{buildroot}/usr/share/doc/ldm/LICENSE %{buildroot}/usr/share/doc/ldm/THIRD_PARTY_NOTICES \
   %{buildroot}%{_defaultlicensedir}/%{name}/

%files
%license %{_defaultlicensedir}/%{name}/LICENSE
%license %{_defaultlicensedir}/%{name}/THIRD_PARTY_NOTICES
%doc /usr/share/doc/ldm/README.md
%dir /usr/share/doc/ldm
/usr/bin/ldmd
/usr/bin/ldm
/usr/bin/ldm-gui
/usr/bin/ldm-dbus
/usr/lib/ldm/
/usr/lib/systemd/user/ldmd.service
/usr/share/applications/ldm.desktop
/usr/share/icons/hicolor/128x128/apps/ldm.png
/usr/share/metainfo/io.github.wzk4work.LDM.metainfo.xml
%dir /etc/opt/chrome/native-messaging-hosts
%dir /etc/chromium/native-messaging-hosts
%dir /etc/opt/edge/native-messaging-hosts
%dir /usr/lib/mozilla/native-messaging-hosts
/etc/opt/chrome/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json
/etc/chromium/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json
/etc/opt/edge/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json
/usr/lib/mozilla/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json

