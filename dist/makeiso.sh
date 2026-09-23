#!/bin/bash
set -euo pipefail

# ==== CONFIGURATION ====
UBUNTU_ISO_URL="https://releases.ubuntu.com/26.04.1/ubuntu-26.04.1-live-server-amd64.iso"
AREXIBO_REPO="romoloman/arexibo"
ISO_OUT="ubuntu-26.04-autoinstall-arexibo.iso"

WORK_DIR="$(pwd)/makeiso-work"
ISO_IN="$WORK_DIR/$(basename "$UBUNTU_ISO_URL")"

# ==== Check required tools ====
for cmd in xorriso 7z openssl curl; do
  if ! command -v "$cmd" &> /dev/null; then
    echo "ERROR: '$cmd' is not installed."
    exit 1
  fi
done

mkdir -p "$WORK_DIR"

# ==== 1. Download the Ubuntu ISO (reused if already present) ====
if [ ! -f "$ISO_IN" ]; then
  echo "=== Downloading Ubuntu ISO ==="
  curl -L --fail -o "$ISO_IN" "$UBUNTU_ISO_URL"
else
  echo "Ubuntu ISO already present: $ISO_IN (delete it to re-download)"
fi

# ==== 2. Arexibo package: use a local .deb if present, otherwise download it ====
LOCAL_DEB=$(ls arexibo*.deb 2>/dev/null | head -1 || true)
if [ -n "$LOCAL_DEB" ]; then
  DEB_FILE="$LOCAL_DEB"
  echo "=== Found $DEB_FILE in the current directory, using it instead of downloading ==="
  if [ "$(ls arexibo*.deb 2>/dev/null | wc -l)" -gt 1 ]; then
    echo "WARNING: multiple arexibo*.deb files found, using the first one alphabetically ($DEB_FILE)."
  fi
else
  read -rp "Arexibo version to install (e.g. 0.6.1): " AREXIBO_VERSION
  DEB_URL="https://github.com/${AREXIBO_REPO}/releases/download/v${AREXIBO_VERSION}/arexibo_${AREXIBO_VERSION}-1_amd64.deb"
  DEB_FILE="$WORK_DIR/arexibo_${AREXIBO_VERSION}-1_amd64.deb"

  echo "=== Downloading arexibo ${AREXIBO_VERSION} ==="
  curl -L --fail -o "$DEB_FILE" "$DEB_URL"
fi

# ==== 3. Totem username and password ====
while true; do
  read -rp "Username for totem access: " TOTEM_USERNAME
  [[ "$TOTEM_USERNAME" =~ ^[a-z_][a-z0-9_-]*$ ]] && break
  echo "Invalid username (lowercase letters/digits/-/_ only, can't start with a digit)."
done

while true; do
  read -rsp "Password: " TOTEM_PASSWORD; echo
  read -rsp "Confirm password: " TOTEM_PASSWORD_CONFIRM; echo
  [ "$TOTEM_PASSWORD" = "$TOTEM_PASSWORD_CONFIRM" ] && break
  echo "The two passwords don't match, try again."
done
# -stdin keeps the password out of the process's own arguments (ps aux)
TOTEM_PASSWORD_HASH=$(openssl passwd -6 -stdin <<< "$TOTEM_PASSWORD")
unset TOTEM_PASSWORD TOTEM_PASSWORD_CONFIRM

# ==== 3b. Keyboard layout, timezone, screen orientation ====
read -rp "Keyboard layout [default: it]: " KEYBOARD_LAYOUT
KEYBOARD_LAYOUT="${KEYBOARD_LAYOUT:-it}"

while true; do
  read -rp "Timezone [default: Europe/Rome]: " TIMEZONE
  TIMEZONE="${TIMEZONE:-Europe/Rome}"
  if [ -f "/usr/share/zoneinfo/$TIMEZONE" ]; then
    break
  fi
  echo "Unknown timezone '$TIMEZONE' (not found under /usr/share/zoneinfo on this machine). Try again."
done

read -rp "Screen orientation, landscape or portrait? [L/p] [default: landscape]: " ORIENTATION_ANSWER
if [[ "$ORIENTATION_ANSWER" =~ ^[pP] ]]; then
  SCREEN_ROTATE_OPTION='Option      "Rotate" "left"'
else
  SCREEN_ROTATE_OPTION=""
fi

# ==== 4. Optional WireGuard VPN ====
read -rp "Enable automatic WireGuard VPN provisioning? [y/N]: " WG_ANSWER
ENABLE_WIREGUARD=0
[[ "$WG_ANSWER" =~ ^[yY] ]] && ENABLE_WIREGUARD=1

# ==== 4b. Xibo CMS registration (optional) ====
read -rp "Xibo server URL (leave empty to use Register via Code): " XIBO_HOST
read -rp "Xibo CMS key (leave empty if not registering right away): " XIBO_KEY

if [ -n "$XIBO_HOST" ] && [ -n "$XIBO_KEY" ]; then
  XIBO_REGISTER_LINE="runuser -u arexibo -- arexibo --host=${XIBO_HOST} --key ${XIBO_KEY} /var/lib/arexibo"
else
  echo "Xibo URL/key omitted: on first boot the arexibo service will start" \
       "directly in \"Register via Code\" mode, with no automatic registration."
  XIBO_REGISTER_LINE=""
fi

# ==== 5. Generate user-data (its content is embedded in this very script) ====
GENERATED_USER_DATA="$WORK_DIR/user-data"
cat > "$GENERATED_USER_DATA" << 'USERDATA_TEMPLATE_EOF'
#cloud-config
autoinstall:
  version: 1
  type: minimized  # Ultra-lightweight minimal install

  locale: it_IT.UTF-8
  keyboard:
    layout: __KEYBOARD_LAYOUT__
    variant: ''
  timezone: __TIMEZONE__

  # Direct guided partitioning (no LVM, no swap)
  storage:
    layout:
      name: direct
    swap:
      size: 0

  # Minimal network for the installer (Ethernet only)
  network:
    version: 2
    renderer: networkd
    ethernets:
      all-eth:
        match:
          name: "e*"
        dhcp4: true
        optional: true

  identity:
    hostname: totem-kiosk
    username: __TOTEM_USERNAME__
    password: "__TOTEM_PASSWORD_HASH__"

  ssh:
    install-server: true
    allow-pw: true

  late-commands:
    # ---------------------------------------------------------
    # 1. WRITING CONFIGURATION FILES DIRECTLY TO DISK (/target)
    # ---------------------------------------------------------
    
    # Ethernet netplan
    - mkdir -p /target/etc/netplan
    - |
      cat << 'EOF' > /target/etc/netplan/01-kiosk-network.yaml
      network:
        version: 2
        renderer: networkd
        ethernets:
          all-eth:
            match:
              name: "e*"
            dhcp4: true
            optional: true
      EOF
    - chmod 600 /target/etc/netplan/01-kiosk-network.yaml

    # NOTE: no netplan file for WiFi. A netplan file declaring wlp3s0
    # under "wifis:" without "access-points:" causes a known, documented
    # NetworkManager crash (assert in nms-keyfile-writer.c:551, Launchpad
    # bug #2038811/#1998207/#2055148) the moment nmcli tries to create a
    # real connection on that same interface. The network-manager
    # package ships /usr/lib/NetworkManager/conf.d/10-globally-managed-
    # devices.conf by default, which makes NM manage every interface not
    # claimed by networkd: just don't mention wlp3s0 in any netplan file
    # at all, and NM picks it up on its own.

    # WiFi/4G modem configuration script, callable via SSH
    - mkdir -p /target/usr/local/sbin
    - |
      cat << 'EOF' > /target/usr/local/sbin/totem-net-config
      #!/usr/bin/env bash
      #
      # totem-net-config
      # Menu utility to configure WiFi or a 4G modem on T-Max Lab totems.
      # Requires: network-manager (nmcli), whiptail, modemmanager, usb-modeswitch.
      #
      # Usage: run with sudo over SSH:
      #   sudo totem-net-config
      #
      set -euo pipefail

      WHIPTAIL_H=20
      WHIPTAIL_W=70

      require_root() {
          if [[ "${EUID}" -ne 0 ]]; then
              echo "This script requires root privileges (use: sudo totem-net-config)" >&2
              exit 1
          fi
      }

      require_cmd() {
          local missing=()
          for c in "$@"; do
              command -v "$c" >/dev/null 2>&1 || missing+=("$c")
          done
          if [[ ${#missing[@]} -gt 0 ]]; then
              echo "Missing commands: ${missing[*]}" >&2
              echo "Install with: sudo apt install network-manager whiptail modemmanager usb-modeswitch" >&2
              exit 1
          fi
      }

      check_networkmanager_active() {
          if ! systemctl is-active --quiet NetworkManager; then
              whiptail --title "NetworkManager not active" --msgbox \
      "NetworkManager does not appear to be active on this system.

      Check that it's installed and enabled:
        sudo systemctl enable --now NetworkManager

      The network-manager package manages, by default, every
      interface not claimed by systemd-networkd." \
              "$WHIPTAIL_H" "$WHIPTAIL_W"
              exit 1
          fi
      }

      pause() {
          whiptail --title "$1" --msgbox "$2" "$WHIPTAIL_H" "$WHIPTAIL_W"
      }

      menu_wifi() {
          whiptail --title "WiFi scan" --infobox "Scanning for networks..." 8 50
          nmcli device wifi rescan >/dev/null 2>&1 || true
          sleep 3

          # In-memory array: SSID<TAB>SIGNAL<TAB>SECURITY per line. Using
          # "< <(...)" (process substitution), not a pipe, otherwise the
          # while loop would run in a subshell and the array wouldn't be
          # visible afterward.
          local wifi_lines=()
          while IFS=$'\t' read -r ssid signal security; do
              [[ -n "$ssid" ]] && wifi_lines+=("$ssid"$'\t'"$signal"$'\t'"$security")
          done < <(nmcli -t -f SSID,SIGNAL,SECURITY device wifi list | \
                       awk -F: '$1!="" {print $1"\t"$2"%\t"$3}' | sort -u)

          if [[ ${#wifi_lines[@]} -eq 0 ]]; then
              pause "WiFi" "No networks found. Check that the WiFi adapter is present and not blocked (rfkill).\n\nIf the network is on 5GHz and doesn't show up, try again: some channels need a longer passive scan."
              return
          fi

          local menu_items=()
          local i=0
          for line in "${wifi_lines[@]}"; do
              IFS=$'\t' read -r ssid signal security <<< "$line"
              menu_items+=("$i" "$ssid ($signal, $security)")
              i=$((i+1))
          done

          local choice
          choice=$(whiptail --title "Available WiFi networks" --menu "Select a network:" \
              "$WHIPTAIL_H" "$WHIPTAIL_W" 10 "${menu_items[@]}" 3>&1 1>&2 2>&3) || return
          local ssid security
          IFS=$'\t' read -r ssid _ security <<< "${wifi_lines[$choice]}"

          local password=""
          if [[ "$security" != "--" && -n "$security" ]]; then
              password=$(whiptail --title "WiFi password" --passwordbox \
                  "Password for network \"$ssid\" ($security):" \
                  "$WHIPTAIL_H" "$WHIPTAIL_W" 3>&1 1>&2 2>&3) || return
          fi

          # Remove any previous connection with the same name to avoid conflicts
          nmcli connection delete "$ssid" >/dev/null 2>&1 || true

          # Explicitly set key-mgmt instead of letting nmcli guess it from
          # the scan: on 5GHz/DFS networks the scan sometimes doesn't
          # correctly report the security parameters at connection time,
          # causing the "802-11-wireless-security.key-mgmt: property is
          # missing" error with the implicit "nmcli device wifi connect"
          # method.
          local add_output up_output ok
          if [[ -z "$password" ]]; then
              add_output=$(nmcli connection add type wifi con-name "$ssid" ifname '*' ssid "$ssid" 2>&1) && ok=1 || ok=0
          else
              add_output=$(nmcli connection add type wifi con-name "$ssid" ifname '*' ssid "$ssid" \
                  wifi-sec.key-mgmt wpa-psk wifi-sec.psk "$password" 2>&1) && ok=1 || ok=0
          fi

          if [[ "$ok" -ne 1 ]]; then
              pause "WiFi - error" "Failed to create the connection profile:\n\n$add_output"
              return
          fi

          if up_output=$(nmcli connection up "$ssid" 2>&1); then
              ensure_wifi_wait_online_override
              pause "WiFi" "Connected to \"$ssid\" successfully.\n\n$up_output"
          else
              pause "WiFi - error" "Profile created but connection failed:\n\n$up_output\n\nIf the network is WPA3-only (not mixed WPA2/WPA3), you may need key-mgmt sae instead of wpa-psk: contact support."
          fi
      }

      # NetworkManager-wait-online.service by default uses `nm-online -s
      # -q` (--wait-for-startup): it only waits for NetworkManager to have
      # ATTEMPTED to bring up known connections, not for connectivity to
      # actually be established -- it can report success while WiFi is
      # still negotiating DHCP/authentication, letting arexibo (which
      # depends on network-online.target) start before the network is
      # really ready. Applied only here, not unconditionally on every
      # totem regardless of network configuration -- an Ethernet-only
      # totem gets no benefit from this more precise wait, and there's no
      # point making it wait needlessly on every boot for a connection
      # type it doesn't use.
      ensure_wifi_wait_online_override() {
          local override_dir=/etc/systemd/system/NetworkManager-wait-online.service.d
          local override_file="$override_dir/override.conf"
          if [[ -f "$override_file" ]]; then
              return
          fi
          mkdir -p "$override_dir"
          cat << 'INNEREOF' > "$override_file"
      [Service]
      ExecStart=
      ExecStart=/usr/bin/nm-online -q --timeout=20
      INNEREOF
          systemctl daemon-reload
      }

      menu_modem() {
          if ! command -v mmcli >/dev/null 2>&1; then
              pause "4G modem" "mmcli not found. Install modemmanager."
              return
          fi

          local modem_list
          modem_list=$(mmcli -L 2>&1 || true)
          if [[ "$modem_list" == *"No modems"* ]] || [[ -z "$modem_list" ]]; then
              pause "4G modem" "No modem detected by ModemManager.\n\nCheck:\n- that the dongle is plugged in\n- that usb_modeswitch has switched it into modem mode (dmesg | tail)\n- lsusb to see whether it shows up as storage or as a modem"
              return
          fi

          local apn pin
          apn=$(whiptail --title "4G modem - APN" --inputbox \
              "Enter the APN provided by the carrier (e.g. mobile.vodafone.it):" \
              "$WHIPTAIL_H" "$WHIPTAIL_W" 3>&1 1>&2 2>&3) || return

          if [[ -z "$apn" ]]; then
              pause "4G modem" "Empty APN, operation cancelled."
              return
          fi

          pin=$(whiptail --title "4G modem - SIM PIN" --passwordbox \
              "SIM PIN (leave empty if not required):" \
              "$WHIPTAIL_H" "$WHIPTAIL_W" 3>&1 1>&2 2>&3) || return

          if [[ -n "$pin" ]]; then
              local modem_idx
              modem_idx=$(mmcli -L | grep -oP '(?<=/Modem/)\d+' | head -1)
              if [[ -n "$modem_idx" ]]; then
                  mmcli -m "$modem_idx" --pin="$pin" >/dev/null 2>&1 || \
                      pause "4G modem" "Warning: sending the PIN to the modem failed or it's already unlocked. Continuing with APN configuration."
              fi
          fi

          # Remove any previous gsm connection with the same name to avoid duplicates
          nmcli connection delete "modem4g" >/dev/null 2>&1 || true

          local output
          if output=$(nmcli connection add type gsm ifname '*' con-name "modem4g" apn "$apn" 2>&1); then
              if output2=$(nmcli connection up "modem4g" 2>&1); then
                  pause "4G modem" "GSM connection configured and active.\n\n$output2"
              else
                  pause "4G modem - warning" "Connection created but not activated:\n\n$output2\n\nCheck APN, PIN, and signal coverage."
              fi
          else
              pause "4G modem - error" "Failed to create the GSM connection:\n\n$output"
          fi
      }

      menu_status() {
          local dev_status nm_conns modem_info ip_info
          dev_status=$(nmcli device status 2>&1 || echo "nmcli not available")
          ip_info=$(ip -brief addr 2>&1 || true)
          modem_info=$(mmcli -L 2>&1 || echo "mmcli not available")

          whiptail --title "Network status" --scrolltext --msgbox \
      "=== nmcli device status ===
      $dev_status

      === ip addr ===
      $ip_info

      === Modem (mmcli -L) ===
      $modem_info" \
          "$WHIPTAIL_H" "$WHIPTAIL_W"
      }

      # Detects whether a WiFi device managed by NetworkManager is
      # present -- not whether a network is currently reachable/
      # configured, only whether the hardware itself exists (a missing
      # WiFi adapter shouldn't show the menu entry at all, a field
      # technician shouldn't see an option for hardware the totem
      # doesn't have).
      has_wifi_hardware() {
          nmcli device status 2>/dev/null | awk '$2 == "wifi" { found=1 } END { exit !found }'
      }

      # Same logic for the 4G modem, reusing the same check already
      # present inside menu_modem() -- if mmcli sees no modem, the
      # hardware isn't (yet) present/detected.
      has_modem_hardware() {
          local modem_list
          modem_list=$(mmcli -L 2>&1 || true)
          [[ "$modem_list" != *"No modems"* ]] && [[ -n "$modem_list" ]]
      }

      main_menu() {
          while true; do
              local menu_items=() item_count=0

              if has_wifi_hardware; then
                  menu_items+=("wifi" "Configure WiFi")
                  item_count=$((item_count + 1))
              fi

              if has_modem_hardware; then
                  menu_items+=("modem" "Configure 4G modem")
                  item_count=$((item_count + 1))
              fi

              menu_items+=("status" "Network status")
              menu_items+=("exit" "Exit")
              ((item_count += 2))

              local choice
              choice=$(whiptail --title "Totem network configuration" --menu \
                  "Select an operation:" "$WHIPTAIL_H" "$WHIPTAIL_W" "$item_count" \
                  "${menu_items[@]}" \
                  3>&1 1>&2 2>&3) || break

              case "$choice" in
                  wifi) menu_wifi ;;
                  modem) menu_modem ;;
                  status) menu_status ;;
                  exit) break ;;
              esac
          done
      }

      require_root
      require_cmd nmcli whiptail mmcli
      check_networkmanager_active
      main_menu
      EOF
    - chmod 755 /target/usr/local/sbin/totem-net-config

    # "tecnici" user: SSH access for network configuration only. Can run
    # in sudo EXCLUSIVELY /usr/local/sbin/totem-net-config, no other
    # command. Not added to the sudo/admin group: the permission comes
    # solely from the dedicated sudoers rule below, so it has no generic
    # sudo access in any other way.
    #
    # NOTE: sudo by default requires the password of the user CALLING
    # sudo (tecnici itself), not root's. The tecnici user will use their
    # own password both for SSH login and for sudo authentication.
    #
    # NOTE 2: this rule only limits privilege escalation to root, not the
    # user's own shell: with a normal bash shell, tecnici can still browse
    # the filesystem and run unprivileged commands. If tighter confinement
    # is needed (e.g. a login that only runs the script then disconnects),
    # it needs to be added separately by changing the login shell.
    - curtin in-target -- useradd -m -s /bin/bash -p '$6$a56b7f97374c4b59$ysD5mVK3EeNV1zhjlRe/Jf5/KbwZjdj9V2pfN2jFfD7ahfHaH2T6KO5Pz4FTvmSAMTUIfHcQkMvTS.fXGacqX0' tecnici
    - |
      cat << 'EOF' > /target/etc/sudoers.d/tecnici-totem-net
      # Sudo commands allowed for the tecnici user (contracted
      # maintainers). Each line is one specific command, not generic
      # sudo. Do not add "ALL" or overly broad wildcards without
      # evaluating the implications: each line below was chosen because
      # it's limited to one precise action, even where it uses a
      # trailing asterisk.
      tecnici ALL=(root) /usr/local/sbin/totem-net-config
      tecnici ALL=(root) /usr/sbin/reboot
      tecnici ALL=(root) /usr/bin/systemctl restart arexibo.service
      tecnici ALL=(root) /usr/bin/systemctl status arexibo.service
      tecnici ALL=(root) /usr/bin/journalctl -u arexibo.service *
      EOF
    - chmod 440 /target/etc/sudoers.d/tecnici-totem-net
    - curtin in-target -- visudo -cf /etc/sudoers.d/tecnici-totem-net

    # XORG CONFIGURATION (detected GPU driver + TearFree where supported)
    - mkdir -p /target/etc/X11/xorg.conf.d
    - |
      GPU_VENDOR=$(cat /target/tmp/gpu-vendor 2>/dev/null || echo generic)
      case "$GPU_VENDOR" in
        intel)
          XORG_DRIVER="intel"
          XORG_EXTRA='    Option      "TearFree" "true"
          Option      "AccelMethod" "sna"
          Option      "DRI" "3"'
          ;;
        amd)
          XORG_DRIVER="amdgpu"
          XORG_EXTRA='    Option      "TearFree" "true"'
          ;;
        nvidia)
          # nouveau doesn't expose the same tuning options as intel/amdgpu
          XORG_DRIVER="nouveau"
          XORG_EXTRA=""
          ;;
        *)
          XORG_DRIVER="modesetting"
          XORG_EXTRA=""
          ;;
      esac
      cat << XORGEOF > /target/etc/X11/xorg.conf.d/20-gpu-tearfree.conf
      Section "Device"
          Identifier  "GPU"
          Driver      "$XORG_DRIVER"
      $XORG_EXTRA
      EndSection

      Section "Monitor"
          Identifier  "Monitor0"
          __SCREEN_ROTATE_OPTION__
      EndSection

      Section "Screen"
          Identifier  "Screen0"
          Device      "GPU"
          Monitor     "Monitor0"
      EndSection
      XORGEOF
    - chmod 644 /target/etc/X11/xorg.conf.d/20-gpu-tearfree.conf

    # Xorg permissions
    - |
      cat << 'EOF' > /target/etc/X11/Xwrapper.config
      allowed_users=anybody
      needs_root_rights=yes
      EOF
    - chmod 644 /target/etc/X11/Xwrapper.config

    # Disable DontVTSwitch
    - mkdir -p /target/etc/X11/xorg.conf.d
    - |
      cat << 'EOF' > /target/etc/X11/xorg.conf.d/10-dontvtswitch.conf
      Section "ServerFlags"
          Option "DontVTSwitch" "true"
      EndSection
      EOF
    - chmod 644 /target/etc/X11/xorg.conf.d/10-dontvtswitch.conf

    # Journald max 50M
    - mkdir -p /target/etc/systemd/journald.conf.d
    - |
      cat << 'EOF' > /target/etc/systemd/journald.conf.d/kiosk.conf
      [Journal]
      SystemMaxUse=50M
      EOF
    - chmod 644 /target/etc/systemd/journald.conf.d/kiosk.conf

    # FIRSTBOOT_BLOCK_START -- makeiso.sh replaces everything between
    # these two markers (inclusive) with a plain "systemctl enable
    # arexibo.service" when no Xibo registration (URL+key) is provided.
    # Arexibo first-boot script
    - mkdir -p /target/usr/local/bin
    - |
      cat << 'EOF' > /target/usr/local/bin/arexibo-firstboot.sh
      #!/bin/bash
      sleep 5
      __XIBO_REGISTER_LINE__
      systemctl enable --now arexibo.service
      rm -f /etc/systemd/system/multi-user.target.wants/arexibo-firstboot.service
      EOF
    - chmod 755 /target/usr/local/bin/arexibo-firstboot.sh

    # Arexibo first-boot systemd service
    - mkdir -p /target/etc/systemd/system
    - |
      cat << 'EOF' > /target/etc/systemd/system/arexibo-firstboot.service
      [Unit]
      Description=Arexibo First Boot Registration
      After=network-online.target
      Wants=network-online.target

      [Service]
      Type=oneshot
      RemainAfterExit=yes
      ExecStart=/usr/local/bin/arexibo-firstboot.sh

      [Install]
      WantedBy=multi-user.target
      EOF
    - chmod 644 /target/etc/systemd/system/arexibo-firstboot.service

    # Enable the first-boot service via symlink
    - mkdir -p /target/etc/systemd/system/multi-user.target.wants
    - ln -sf /etc/systemd/system/arexibo-firstboot.service /target/etc/systemd/system/multi-user.target.wants/arexibo-firstboot.service
    # FIRSTBOOT_BLOCK_END

    # WIREGUARD_BLOCK_START -- makeiso.sh removes everything between
    # these two markers (inclusive) when VPN provisioning is disabled.
    # Do not move or rename these comments without updating the script.
    # ---------------------------------------------------------
    # WIREGUARD VPN PROVISIONING SCRIPT AND SERVICE
    # ---------------------------------------------------------
    # Auto-registers the totem on the management VPN, kept separate from
    # traffic to the Xibo CMS. The WireGuard private key is generated
    # locally on the totem on the first useful boot and never leaves the
    # machine -- only the public key is sent to the provisioning
    # service, which associates it with the totem's hardwareKey (read
    # from /var/lib/arexibo/cms.json, written by arexibo-firstboot.sh
    # above) only if the Display is authorized on Xibo. Requires no
    # action from the technician installing the OS.
    #
    # The CMS/provisioning service address is NOT hardcoded: it's read
    # from the same cms.json file (the "address" field, written/updated
    # by arexibo itself), so if the CMS URL ever changes there's no need
    # to touch this ISO image. As a useful side effect, this also lets
    # us detect a totem migrating to a different CMS instance: the
    # .provisioned marker isn't just a plain empty flag anymore, it holds
    # the CMS address provisioning was done against -- if a later boot
    # finds it different from the current one in cms.json, the script
    # redoes provisioning from scratch against the new CMS (new key, new
    # IP, new endpoint) instead of silently staying attached to the
    # previous CMS's VPN.
    #
    # NOTE/known limit: this does not de-provision the peer on the OLD
    # WireGuard server in case of migration -- the totem simply stops
    # using it. It stays there as an orphaned entry until manually
    # cleaned up (or by a future registry<->Xibo sync script, not yet
    # built).
    #
    # The Type=oneshot with Restart=on-failure/RestartSec=60 handles on
    # its own the case where the totem powers on BEFORE an operator
    # authorizes it on Xibo (the normal flow in the field): the script
    # fails with exit 1 and retries every 60s, with no technician
    # intervention, until it gets a 200 response from the provisioning
    # service -- at that point it writes the .provisioned marker and
    # doesn't run again (unless a CMS change is detected, as above).
    - mkdir -p /target/usr/local/bin
    - |
      cat << 'EOF' > /target/usr/local/bin/vpn-provision.sh
      #!/bin/bash
      set -euo pipefail

      WG_DIR="/etc/wireguard"
      MARKER="$WG_DIR/.provisioned"
      CMS_JSON="/var/lib/arexibo/cms.json"

      if [ ! -f "$CMS_JSON" ]; then
          echo "arexibo not yet initialized (cms.json missing), will retry later" >&2
          exit 1
      fi

      HW_KEY=$(jq -r '.display_id // empty' "$CMS_JSON")
      if [ -z "$HW_KEY" ]; then
          echo "hardwareKey not yet present in cms.json, will retry later" >&2
          exit 1
      fi

      CMS_ADDRESS=$(jq -r '.address // empty' "$CMS_JSON")
      if [ -z "$CMS_ADDRESS" ]; then
          echo "'address' field not present in cms.json, will retry later" >&2
          exit 1
      fi
      # normalize by stripping any trailing slash, for reliable
      # comparisons with the marker's content and to build the URL
      CMS_ADDRESS="${CMS_ADDRESS%/}"
      PROVISION_URL="$CMS_ADDRESS/provision"

      if [ -f "$MARKER" ]; then
          PREVIOUS_ADDRESS=$(cat "$MARKER")
          if [ "$PREVIOUS_ADDRESS" = "$CMS_ADDRESS" ]; then
              exit 0
          fi
          echo "CMS changed ($PREVIOUS_ADDRESS -> $CMS_ADDRESS): redoing VPN provisioning." >&2
          # don't exit: continue to re-provision against the new CMS
      fi

      mkdir -p "$WG_DIR"
      chmod 700 "$WG_DIR"

      if [ ! -f "$WG_DIR/privatekey" ]; then
          wg genkey | tee "$WG_DIR/privatekey" > /dev/null
          chmod 600 "$WG_DIR/privatekey"
      fi
      PUBKEY=$(wg pubkey < "$WG_DIR/privatekey")

      HTTP_CODE=$(curl -s -o /tmp/provision-resp.json -w "%{http_code}" \
          -X POST "$PROVISION_URL" \
          -H "Content-Type: application/json" \
          -d "{\"hardware_key\":\"$HW_KEY\",\"pubkey\":\"$PUBKEY\"}")

      case "$HTTP_CODE" in
          200) ;;
          403)
              echo "Display not yet authorized on Xibo, will retry later" >&2
              exit 1
              ;;
          404)
              echo "Display not found on Xibo, will retry later" >&2
              exit 1
              ;;
          *)
              echo "Unexpected error from the provisioning service: HTTP $HTTP_CODE" >&2
              cat /tmp/provision-resp.json >&2
              exit 1
              ;;
      esac

      ADDRESS=$(jq -r '.address' /tmp/provision-resp.json)
      SERVER_PUBKEY=$(jq -r '.server_pubkey' /tmp/provision-resp.json)
      ENDPOINT=$(jq -r '.endpoint' /tmp/provision-resp.json)
      rm -f /tmp/provision-resp.json

      cat > "$WG_DIR/wg0.conf" << INNEREOF
      [Interface]
      PrivateKey = $(cat "$WG_DIR/privatekey")
      Address = $ADDRESS

      [Peer]
      PublicKey = $SERVER_PUBKEY
      Endpoint = $ENDPOINT
      AllowedIPs = 10.8.0.0/23
      PersistentKeepalive = 25
      INNEREOF

      chmod 600 "$WG_DIR/wg0.conf"
      rm -f "$WG_DIR/privatekey"

      # enable (idempotent) + restart (not just "start") so that, in
      # case of CMS migration, an already-active interface gets reloaded
      # with the new configuration instead of staying attached to the
      # previous one
      systemctl enable wg-quick@wg0
      systemctl restart wg-quick@wg0

      sleep 2
      if wg show wg0 > /dev/null 2>&1; then
          echo "$CMS_ADDRESS" > "$MARKER"
          echo "VPN provisioning complete."
          exit 0
      else
          echo "wg0 not active after provisioning" >&2
          exit 1
      fi
      EOF
    - chmod 755 /target/usr/local/bin/vpn-provision.sh

    # VPN provisioning systemd service
    #
    # IMPORTANT NOTE: this is a Type=oneshot WITHOUT RemainAfterExit --
    # after a successful run (exit 0), systemd marks it "inactive
    # (dead)" and Restart=on-failure will NEVER restart it again (it
    # only fires on failures, not after a success). For this reason the
    # service is NOT enabled/started directly: it's invoked exclusively
    # by the vpn-provision.timer below, which calls it back at regular
    # intervals forever -- both for the initial retry before
    # authorization on Xibo, and for the periodic check for a possible
    # CMS change (see the comments in the script). The script itself
    # exits quickly, with no network call at all, when there's nothing
    # to do -- a check every 10 minutes costs very little even across a
    # fleet of hundreds of totems.
    - mkdir -p /target/etc/systemd/system
    - |
      cat << 'EOF' > /target/etc/systemd/system/vpn-provision.service
      [Unit]
      Description=Totem VPN provisioning (WireGuard) - invoked by vpn-provision.timer
      After=network-online.target arexibo-firstboot.service
      Wants=network-online.target

      [Service]
      Type=oneshot
      ExecStart=/usr/local/bin/vpn-provision.sh
      EOF
    - chmod 644 /target/etc/systemd/system/vpn-provision.service

    # Timer that calls the service above every 10 minutes, forever --
    # not just until it succeeds once, but indefinitely: this is what
    # makes detecting a CMS change (described in the script) possible,
    # not just the initial pre-authorization retry. 10 minutes is more
    # than enough: a migration between CMS instances is a rare event,
    # there's no need to detect it near-real-time.
    - |
      cat << 'EOF' > /target/etc/systemd/system/vpn-provision.timer
      [Unit]
      Description=Periodically runs the totem's VPN check/provisioning

      [Timer]
      OnBootSec=30s
      OnUnitActiveSec=10min
      Unit=vpn-provision.service

      [Install]
      WantedBy=timers.target
      EOF
    - chmod 644 /target/etc/systemd/system/vpn-provision.timer

    # Enable the TIMER (not the service) via symlink
    - mkdir -p /target/etc/systemd/system/timers.target.wants
    - ln -sf /etc/systemd/system/vpn-provision.timer /target/etc/systemd/system/timers.target.wants/vpn-provision.timer
    # WIREGUARD_BLOCK_END

    # ---------------------------------------------------------
    # 2. SYSTEM OPERATIONS AND CLEANUP
    # ---------------------------------------------------------

    # MANUAL TIMEZONE SETUP (chroot-proof)
    - ln -sf /usr/share/zoneinfo/__TIMEZONE__ /target/etc/localtime
    - echo "__TIMEZONE__" > /target/etc/timezone

    # KEYBOARD LAYOUT CONFIGURATION (console + X11)
    # 1. Layout for the Linux console (TTY)
    - |
      cat << 'EOF' > /target/etc/default/keyboard
      XKBMODEL="pc105"
      XKBLAYOUT="__KEYBOARD_LAYOUT__"
      XKBVARIANT=""
      XKBOPTIONS=""
      BACKSPACE="guess"
      EOF
    - chmod 644 /target/etc/default/keyboard

    # 2. Layout for X11 (Arexibo / Xorg)
    - mkdir -p /target/etc/X11/xorg.conf.d
    - |
      cat << 'EOF' > /target/etc/X11/xorg.conf.d/00-keyboard.conf
      Section "InputClass"
              Identifier "system-keyboard"
              MatchIsKeyboard "on"
              Option "XkbLayout" "__KEYBOARD_LAYOUT__"
      EndSection
      EOF
    - chmod 644 /target/etc/X11/xorg.conf.d/00-keyboard.conf

    # systemd-networkd-wait-online.service: the LAN on these totems is
    # managed by systemd-networkd (see renderer: networkd above), NOT by
    # NetworkManager -- the right service that wired-network readiness
    # depends on. The default behavior waits for ALL interfaces matching
    # "e*" to come online, up to 120s -- if the totem has multiple
    # physical Ethernet ports and only one is plugged in, it blocks
    # boot pointlessly. On the other hand, --any alone (with no timeout
    # limit) would be dangerous on 4G-modem-only totems (no Ethernet
    # interface plugged in at all, the modem is handled separately by
    # NetworkManager/ModemManager) -- --any alone would still wait up to
    # the default timeout for an interface that will never come online.
    #
    # Fix: a combination of --any (succeeds immediately as soon as AT
    # LEAST ONE connected interface reaches the online state, instead of
    # waiting for all of them) AND a short 8s timeout (limits the cost
    # on modem-only totems, where no Ethernet interface will ever come
    # online, to a bounded delay instead of the default 120s).
    # network-online.target depends on this service via Wants= (a weak
    # coupling) -- a timeout here doesn't permanently block boot, it
    # only adds this bounded delay before the target is reached anyway.
    - mkdir -p /target/etc/systemd/system/systemd-networkd-wait-online.service.d
    - |
      cat << 'EOF' > /target/etc/systemd/system/systemd-networkd-wait-online.service.d/override.conf
      [Service]
      ExecStart=
      ExecStart=/usr/lib/systemd/systemd-networkd-wait-online --any --timeout=8
      EOF
    - chmod 644 /target/etc/systemd/system/systemd-networkd-wait-online.service.d/override.conf

    # NetworkManager-wait-online.service by default uses `nm-online -s
    # -q` (--wait-for-startup): it only waits for NetworkManager to have
    # ATTEMPTED to bring up known connections, not for connectivity to
    # actually be established -- it can report success while the
    # interface (LAN or WiFi) is still negotiating DHCP/DNS, letting
    # arexibo (which depends on network-online.target) start before the
    # network is really ready. Applied here, unconditionally on every
    # totem during provisioning -- originally applied only inside
    # totem-net-config when configuring WiFi, but confirmed that the
    # exact same problem shows up with LAN alone too: it's not WiFi-
    # specific, it's a structural limit of NetworkManager-wait-online's
    # default check for any interface type.
    - mkdir -p /target/etc/systemd/system/NetworkManager-wait-online.service.d
    - |
      cat << 'EOF' > /target/etc/systemd/system/NetworkManager-wait-online.service.d/override.conf
      [Service]
      ExecStart=
      ExecStart=/usr/bin/nm-online -q --timeout=20
      EOF
    - chmod 644 /target/etc/systemd/system/NetworkManager-wait-online.service.d/override.conf

    # Fixed global DNS (Google: 8.8.8.8/8.8.4.4) for systemd-resolved,
    # independent of the active network interface -- fixes a problem
    # found on a real totem connected via 4G modem (Quectel, cdc_mbim
    # driver): the modem's data interface (wwp0sXXXXXX) isn't a
    # "device" managed by NetworkManager (ModemManager itself assigns
    # it an IP via its own internal DHCP client), so the DNS servers
    # provided by the carrier over DHCP never reach systemd-resolved
    # for that link (resolvectl status showed "Current Scopes: none",
    # no DNS Server listed) -- a NetworkManager profile "activated"
    # successfully, but with no working name resolution as soon as the
    # only other interface (Ethernet) got unplugged. A global DNS in
    # resolved.conf is used IN ADDITION to per-link ones (not just as a
    # fallback when everything else is missing), so it fixes both the
    # Ethernet-only and the modem-only case without having to diagnose,
    # case by case, which DNS servers the installed SIM's carrier
    # provides (confirmed: the carrier's own DNS servers, when reached
    # via an explicit route to the modem, responded correctly -- the
    # problem is only propagation to resolved, not reachability).
    - sed -i 's/^#\?DNS=.*/DNS=8.8.8.8 8.8.4.4/' /target/etc/systemd/resolved.conf

    # ModemManager takes up to 90s (systemd's default TimeoutStopSec) to
    # shut down when an active Quectel 4G modem is present, blocking the
    # totem's shutdown/reboot for that same amount of time -- confirmed
    # in the logs of a real totem:
    #   ModemManager[N]: <wrn> shutdown failed: timeout waiting for
    #   sleep preparation to complete
    #   systemd[1]: ModemManager.service: State 'stop-sigterm' timed
    #   out. Killing.
    # with a gap of exactly 90s between the initial SIGTERM and
    # systemd's forced SIGKILL. Root cause not isolated with certainty
    # (likely a quectel plugin/cdc_mbim driver bug in how it handles its
    # own sleep inhibitor toward systemd-logind), but since the modem
    # re-registers correctly on every subsequent boot regardless, a
    # "clean" teardown on shutdown isn't needed at all -- we simply
    # reduce how long systemd waits before forcibly killing the
    # process.
    - mkdir -p /target/etc/systemd/system/ModemManager.service.d
    - |
      cat << 'EOF' > /target/etc/systemd/system/ModemManager.service.d/override.conf
      [Service]
      TimeoutStopSec=5
      EOF
    - chmod 644 /target/etc/systemd/system/ModemManager.service.d/override.conf

    # Remove server ballast (iSCSI, software RAID, Multipath, Kdump)
    - curtin in-target -- apt-get purge -y kdump-tools makedumpfile open-iscsi mdadm multipath-tools nvme-cli || true

    # Remove Subiquity's temporary network config
    - rm -f /target/etc/netplan/00-installer-config.yaml

    # Random hostname
    - |
      RANDOM_SUFFIX=$(tr -dc 'a-z0-9' < /dev/urandom | head -c 6)
      NEW_HOST="totem-${RANDOM_SUFFIX}"
      echo "$NEW_HOST" > /target/etc/hostname
      sed -i "s/127.0.1.1.*/127.0.1.1\t$NEW_HOST/" /target/etc/hosts

    # Automatic GPU detection (Intel / AMD / Nvidia-nouveau / generic)
    # The result is written to a file because every late-commands entry
    # runs in its own sub-shell: a variable set here wouldn't be visible
    # to later steps without saving it to disk.
    - |
      GPU_LINE=$(lspci -d "::0300" -n 2>/dev/null | head -1)
      VENDOR_ID=$(echo "$GPU_LINE" | awk '{print $3}' | cut -d: -f1)
      case "$VENDOR_ID" in
        8086) GPU_VENDOR="intel" ;;
        1002|1022) GPU_VENDOR="amd" ;;
        10de) GPU_VENDOR="nvidia" ;;
        *) GPU_VENDOR="generic" ;;
      esac
      echo "$GPU_VENDOR" > /target/tmp/gpu-vendor
      echo "GPU detection: $GPU_LINE -> vendor=$GPU_VENDOR" > /dev/console

    # Package installation
    # Copy and verify the Arexibo package in the target
    - cp /cdrom/nocloud/arexibo.deb /target/tmp/arexibo.deb || cp /cdrom/arexibo.deb /target/tmp/arexibo.deb || true

    # Enable Universe/Multiverse repositories and update APT
    - curtin in-target -- apt-get update || true
    - curtin in-target -- apt-get install -y software-properties-common || true
    - curtin in-target -- add-apt-repository -y universe || true
    - curtin in-target -- add-apt-repository -y multiverse || true
    - curtin in-target -- apt-get update || true

    # ---------------------------------------------------------
    # 2. SINGLE ATOMIC INSTALL (DEB + TOTEM PACKAGES)
    # ---------------------------------------------------------
    # Step A: system packages from the official repositories
    - |
      GPU_VENDOR=$(cat /target/tmp/gpu-vendor 2>/dev/null || echo generic)
      case "$GPU_VENDOR" in
        intel)
          GPU_PACKAGES="xserver-xorg-video-intel intel-media-va-driver i965-va-driver vainfo"
          ;;
        amd)
          GPU_PACKAGES="xserver-xorg-video-amdgpu mesa-va-drivers vainfo"
          ;;
        nvidia)
          # nouveau: no reliable VA-API driver available (Nvidia VA-API
          # video acceleration requires the proprietary driver, not
          # nouveau) -- Xorg video driver only.
          GPU_PACKAGES="xserver-xorg-video-nouveau"
          ;;
        *)
          GPU_PACKAGES="xserver-xorg-video-fbdev"
          ;;
      esac
      curtin in-target -- env DEBIAN_FRONTEND=noninteractive apt-get install -y \
        -o Dpkg::Options::="--force-confdef" \
        -o Dpkg::Options::="--force-confold" \
        curl \
        wget \
        vim \
        htop \
        cabextract \
        wpasupplicant \
        modemmanager \
        usb-modeswitch \
        network-manager \
        whiptail \
        fonts-noto-color-emoji \
        fonts-montserrat \
        libqmi-utils \
        wireguard \
        jq \
        $GPU_PACKAGES || (
          echo "=== REPOSITORY PACKAGE ERROR ===" > /dev/console
          cat /var/log/apt/term.log > /dev/console 2>&1
          sleep 30
          exit 1
        )

    # Enable NetworkManager (just a symlink for the next boot: here we're
    # in an in-target chroot, the service can't be started live) for
    # WiFi/4G modem management via totem-net-config
    - curtin in-target -- systemctl enable NetworkManager || true

    # Step B: install the local Arexibo package (without problematic Dpkg flags)
    - |
      if [ -f /target/tmp/arexibo.deb ]; then
        echo "=== AREXIBO.DEB FOUND - STARTING INSTALLATION ===" > /dev/console
        curtin in-target -- env DEBIAN_FRONTEND=noninteractive apt-get install -y /tmp/arexibo.deb || (
          echo "=== APT ERROR WHILE INSTALLING AREXIBO.DEB ===" > /dev/console
          cat /var/log/apt/term.log > /dev/console 2>&1
          sleep 30
          exit 1
        )
      else
        echo "=== CRITICAL ERROR: /target/tmp/arexibo.deb DOES NOT EXIST! COPY FAILED ===" > /dev/console
        sleep 30
        exit 1
      fi

    # Step C: clean up the temporary file
    - rm -f /target/tmp/arexibo.deb /target/tmp/gpu-vendor

    # Automatic Microsoft font installation
    - curtin in-target -- bash -c 'echo "ttf-mscorefonts-installer msttcorefonts/accepted-mscorefonts-eula select true" | debconf-set-selections'
    - curtin in-target -- apt-get install -y ttf-mscorefonts-installer || true
    - curtin in-target -- dpkg-reconfigure -f noninteractive fontconfig-config || true
    - curtin in-target -- dpkg-reconfigure -f noninteractive fontconfig || true
    - curtin in-target -- fc-cache -fv || true

    # Full package upgrade -- the ISO image is built at a fixed point in
    # time, but the repositories keep updating in the meantime: without
    # this step, a totem installed months after the ISO was created ends
    # up on first boot with dozens of pending updates (observed: 45 on a
    # clean install). Done here, after all the package installs above but
    # before the GRUB section below -- so that if the upgrade touches the
    # kernel, the following update-grub already reflects the actually-
    # installed final version, not the original ISO image's. --force-
    # confdef/--force-confold (same convention already used above)
    # prevent a package with configuration changes from requiring an
    # interactive answer, which would block an install meant to be fully
    # automatic.
    - curtin in-target -- apt-get update || true
    - |
      curtin in-target -- env DEBIAN_FRONTEND=noninteractive apt-get upgrade -y \
        -o Dpkg::Options::="--force-confdef" \
        -o Dpkg::Options::="--force-confold" || true

    # Silent boot
    - rm -f /target/etc/default/grub.d/*
    - sed -i 's/^GRUB_TIMEOUT=.*/GRUB_TIMEOUT=0/' /target/etc/default/grub
    - sed -i 's/^GRUB_CMDLINE_LINUX_DEFAULT=.*/GRUB_CMDLINE_LINUX_DEFAULT="quiet loglevel=0 console=tty3 systemd.show_status=false rd.udev.log_level=0 vt.global_cursor_default=0 fsck.mode=skip"/' /target/etc/default/grub
    - echo "GRUB_TIMEOUT_STYLE=hidden" >> /target/etc/default/grub
    - echo "GRUB_RECORDFAIL_TIMEOUT=0" >> /target/etc/default/grub
    - curtin in-target -- update-grub

    # Mask TTYs
    - sed -i 's/^#NAutoVTs=.*/NAutoVTs=0/' /target/etc/systemd/logind.conf
    - sed -i 's/^#ReserveVT=.*/ReserveVT=0/' /target/etc/systemd/logind.conf
    - curtin in-target -- systemctl mask getty@tty1.service getty@tty2.service getty@tty3.service getty@tty4.service getty@tty5.service getty@tty6.service || true
    - sed -i 's/^Unattended-Upgrade::Automatic-Reboot .*/Unattended-Upgrade::Automatic-Reboot "false";/' /target/etc/apt/apt.conf.d/50unattended-upgrades || true

    # REGENERATE FONTCONFIG CONFIGURATION
    - curtin in-target -- dpkg-reconfigure -f noninteractive fontconfig-config || true
    - curtin in-target -- dpkg-reconfigure -f noninteractive fontconfig || true
    - curtin in-target -- fc-cache -f -v || true

    # Remove cloud-init
    - curtin in-target -- apt-get purge -y cloud-init
    - curtin in-target -- rm -rf /etc/cloud /var/lib/cloud

    # Remove known orphan packages that autoremove doesn't touch --
    # confirmed on a real totem provisioned with this same script:
    # python3-boto3/botocore/s3transfer stay installed even after apt-get
    # autoremove --purge (below), almost certainly because apt's own
    # state marks them as "manually" installed rather than as an
    # automatic dependency of cloud-init, even though it's the latter
    # that pulled them in during provisioning -- autoremove by
    # definition never touches packages marked manual, no matter how
    # many times it's run. Harmless if absent on a future base image
    # (`|| true`), since apt-get purge would otherwise fail on a package
    # that was never installed.
    - curtin in-target -- apt-get purge -y python3-boto3 python3-botocore python3-s3transfer || true

    # Remove snapd -- confirmed on a real totem: no snap installed (`snap
    # list` empty), no real dependency from metapackages actually present
    # on this system (confirmed that ubuntu-server/ubuntu-server-minimal,
    # which declare Depends/Recommends: snapd in the repository, aren't
    # actually installed here). apt purge alone doesn't always clean up
    # snapd's leftover directories, hence the explicit rm -rf.
    - curtin in-target -- apt-get purge -y snapd || true
    - curtin in-target -- rm -rf /var/lib/snapd /snap /var/snap

    # Final cleanup (orphan packages and apt cache)
    - curtin in-target -- apt-get autoremove -y --purge || true
    - curtin in-target -- apt-get clean || true
USERDATA_TEMPLATE_EOF

sed -i "s|__TOTEM_USERNAME__|${TOTEM_USERNAME}|" "$GENERATED_USER_DATA"
sed -i "s|__TOTEM_PASSWORD_HASH__|${TOTEM_PASSWORD_HASH}|" "$GENERATED_USER_DATA"
sed -i "s|__KEYBOARD_LAYOUT__|${KEYBOARD_LAYOUT}|" "$GENERATED_USER_DATA"
sed -i "s|__TIMEZONE__|${TIMEZONE}|" "$GENERATED_USER_DATA"
sed -i "s|__SCREEN_ROTATE_OPTION__|${SCREEN_ROTATE_OPTION}|" "$GENERATED_USER_DATA"

if [ -n "$XIBO_REGISTER_LINE" ]; then
  sed -i "s|__XIBO_REGISTER_LINE__|${XIBO_REGISTER_LINE}|" "$GENERATED_USER_DATA"
else
  echo "=== No Xibo registration: starting the arexibo service directly ==="
  sed -i '/# FIRSTBOOT_BLOCK_START/,/# FIRSTBOOT_BLOCK_END/c\
    - curtin in-target -- systemctl enable arexibo.service' "$GENERATED_USER_DATA"
fi

if [ "$ENABLE_WIREGUARD" -eq 0 ]; then
  echo "=== WireGuard VPN disabled: removing its block from user-data ==="
  sed -i '/# WIREGUARD_BLOCK_START/,/# WIREGUARD_BLOCK_END/d' "$GENERATED_USER_DATA"
  sed -i '/^[[:space:]]*wireguard \\$/d; /^[[:space:]]*jq \\$/d' "$GENERATED_USER_DATA"
fi

# ==== 6. Remaster the ISO ====
ORIG_VOLID=$(xorriso -indev "$ISO_IN" -p2df 2>&1 | grep "Volume id" | cut -d"'" -f2 || true)
if [ -z "$ORIG_VOLID" ]; then
  ORIG_VOLID="Ubuntu-Server 26.04 amd64"
fi

TMP_DIR=$(mktemp -d /tmp/ubuntu-iso-XXXXXX)
trap 'rm -rf "$TMP_DIR"' EXIT

echo "=== Extracting ISO and boot images ==="
7z x "$ISO_IN" -o"$TMP_DIR/iso" > /dev/null

BOOT_DIR="$TMP_DIR/iso/[BOOT]"
MBR_IMG="$TMP_DIR/1-Boot-NoEmul.img"
EFI_IMG="$TMP_DIR/2-Boot-NoEmul.img"

if [ -f "$BOOT_DIR/1-Boot-NoEmul.img" ] && [ -f "$BOOT_DIR/2-Boot-NoEmul.img" ]; then
  mv "$BOOT_DIR/1-Boot-NoEmul.img" "$MBR_IMG"
  mv "$BOOT_DIR/2-Boot-NoEmul.img" "$EFI_IMG"
  rm -rf "$BOOT_DIR"
else
  echo "ERROR: boot images not found."
  exit 1
fi

echo "=== Copying user-data, meta-data and arexibo.deb ==="
mkdir -p "$TMP_DIR/iso/nocloud"
cp "$GENERATED_USER_DATA" "$TMP_DIR/iso/nocloud/user-data"
touch "$TMP_DIR/iso/nocloud/meta-data"
touch "$TMP_DIR/iso/nocloud/vendor-data"
cp "$DEB_FILE" "$TMP_DIR/iso/nocloud/arexibo.deb"

echo "=== Editing GRUB boot command ==="
GRUB_CFG="$TMP_DIR/iso/boot/grub/grub.cfg"
chmod +w "$GRUB_CFG"

sed -i 's|/casper/vmlinuz|/casper/vmlinuz autoinstall "ds=nocloud;s=/cdrom/nocloud/"|g' "$GRUB_CFG"
sed -i 's/set timeout=.*/set timeout=1/g' "$GRUB_CFG"

echo "=== Generating isohybrid ISO ==="
chmod -R +w "$TMP_DIR/iso"

xorriso -as mkisofs -r \
  -V "$ORIG_VOLID" \
  -o "$ISO_OUT" \
  --grub2-mbr "$MBR_IMG" \
  -partition_offset 16 \
  --mbr-force-bootable \
  -append_partition 2 28732ac11ff8d211ba4b00a0c93ec93b "$EFI_IMG" \
  -appended_part_as_gpt \
  -iso_mbr_part_type a2a0d0ebe5b9334487c068b6b72699c7 \
  -c '/boot.catalog' \
  -b '/boot/grub/i386-pc/eltorito.img' \
    -no-emul-boot -boot-load-size 4 -boot-info-table --grub2-boot-info \
  -eltorito-alt-boot \
  -e '--interval:appended_partition_2:all::' \
    -no-emul-boot \
  "$TMP_DIR/iso"

echo "ISO created: $ISO_OUT"
echo "User: $TOTEM_USERNAME  WireGuard VPN: $([ "$ENABLE_WIREGUARD" -eq 1 ] && echo enabled || echo disabled)"
echo "Keyboard: $KEYBOARD_LAYOUT  Timezone: $TIMEZONE  Orientation: $([ -n "$SCREEN_ROTATE_OPTION" ] && echo portrait || echo landscape)"
echo "Xibo registration: $([ -n "$XIBO_REGISTER_LINE" ] && echo "automatic on $XIBO_HOST" || echo "Register via Code (no URL/key provided)")"
