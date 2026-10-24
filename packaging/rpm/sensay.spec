# Packs the files build-release.sh staged in %{sensay_stage}; nothing is
# compiled here.
%global debug_package %{nil}
%global __os_install_post %{nil}
%global __arch_install_post %{nil}
%global _build_id_links none

Name:           sensay
Version:        %{sensay_version}
Release:        1
Summary:        Dictation: hold a key, speak, and the text is pasted
License:        LicenseRef-Proprietary
URL:            https://sensay.app
# Opened while running, so the automatic dependencies miss them.
Requires:       libxkbcommon.so.0()(64bit)
Requires:       libxkbcommon-x11.so.0()(64bit)

%description
Sensay records while the activation key is held, sends the recording to the
speech service and pastes the text where the cursor is.

%install
cp -a %{sensay_stage}/. %{buildroot}/

%post
# The udev rule reaches the devices present now: the user at the seat can
# read the keyboards and use /dev/uinput without logging out.
udevadm control --reload-rules >/dev/null 2>&1 || :
modprobe uinput >/dev/null 2>&1 || :
udevadm trigger --action=change --subsystem-match=input --subsystem-match=misc >/dev/null 2>&1 || :

%postun
udevadm control --reload-rules >/dev/null 2>&1 || :

%files
/usr/bin/sensay
/usr/share/applications/com.advertumtech.Sensay.desktop
/usr/share/icons/hicolor/*/apps/com.advertumtech.Sensay.*
/usr/lib/udev/rules.d/70-sensay.rules
/usr/share/gnome-shell/extensions/sensay-pill@sensay.app
