#!/usr/bin/env bash
# Cut and restore USB power to a board, addressed by its MAC.
#
# The hub can switch ports individually, which is the only way to power-cycle a
# board that has parked itself in the ROM downloader -- nothing in software
# clears `force_download_boot`, so a reset will not do it and neither will
# esptool.
#
# ## Why this addresses boards by MAC and never by port
#
# Port numbers and /dev/ttyACMn are both unstable. Replugging the hub swapped
# the boards on ports 3 and 4 and renumbered every ACM device; a script that
# remembered "port 3" would have cut power to a different board afterwards,
# with nothing to warn it. The MAC is printed on the chip and never moves, so
# it is the only durable name. The port is looked up fresh on every call.
#
# ## The board that must never be cut
#
# /dev/ttyACM0 has been the lightning storm monitor all week -- a *running*
# device belonging to another project. It currently sits on a root port, well
# outside this hub, so it cannot be reached even by accident. That is luck
# rather than design: move it onto the hub and it becomes one typo away from an
# outage. So its MAC is refused explicitly, and the refusal does not depend on
# where it happens to be plugged in.
#
#   tools/power.sh list
#   tools/power.sh cycle ls                   # by name, from devices.list
#   tools/power.sh cycle AC:27:6E:...         # or by MAC, still checked
#
set -euo pipefail

# **Identity comes from `devices.list`, never from the bus.**
#
# A board that is unplugged, asleep, or wedged does not appear in sysfs at all --
# and those are exactly the moments this script is reached for. Resolving a name
# by walking the bus would mean the roster shrinks to whatever happens to be
# answering, so a missing board becomes invisible instead of reported, and the
# `forbidden` flag would stop protecting a device the moment it stopped
# enumerating.
#
# So the file is the roster and the bus is only asked *where* a board is.
# shellcheck source=board.sh
. "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/board.sh"

die() { echo "power: $*" >&2; exit 1; }

# Every Espressif board the kernel can see, as "MAC hub port ttyACMn".
#
# Walks sysfs rather than parsing lsusb because sysfs is where the port number
# actually lives: a device at 5-1.3 hangs off hub 5-1 on port 3. A board sitting
# on a root port (5-2, no dot) has no switchable hub above it and is reported
# with a port of "-".
inventory() {
    local dir name serial hub port tty
    for dir in /sys/bus/usb/devices/*/; do
        [ -f "$dir/serial" ] || continue
        serial=$(<"$dir/serial")
        [[ "$serial" == *:*:*:*:*:* ]] || continue   # MAC-shaped only
        name=$(basename "$dir")
        if [[ "$name" == *.* ]]; then
            hub="${name%.*}"
            port="${name##*.}"
        else
            hub="-"                                   # directly on a root port
            port="-"
        fi
        tty=$(basename "$(echo "$dir"/*/tty/ttyACM* 2>/dev/null | awk '{print $1}')" 2>/dev/null)
        [[ "$tty" == ttyACM* ]] || tty="-"
        echo "$serial $hub $port $tty"
    done
}

# Resolve a name or a MAC to a MAC, uppercased.
#
# A MAC passed straight through is still checked against the roster by the
# caller -- being explicit is not the same as being right, and this script cuts
# power.
resolve() {
    local want="$1" mac
    if [[ "$want" == *:*:*:*:*:* ]]; then
        echo "${want^^}"
        return 0
    fi
    mac="$(board_mac "$want")" || { board_list_missing; exit 1; }
    [ -n "$mac" ] || die "no board named '$want' in devices.list"
    echo "$mac"
}

# **Every board on the roster, present or not, then anything unexpected.**
#
# A listing built only from the bus omits the boards that are missing, which is
# the one thing worth knowing when a board has stopped answering. The roster is
# printed first and absent entries say so; anything plugged in that the roster
# does not know is printed after, because that is also worth knowing.
cmd_list() {
    local list
    list="$(devices_list)" || { board_list_missing; exit 1; }

    local seen="" name mac flags found hub port tty state
    printf "%-12s %-20s %-8s %-6s %-10s %s\n" NAME MAC HUB PORT TTY STATE
    while read -r name mac _rest; do
        [ -n "$name" ] || continue
        mac="${mac^^}"
        seen="$seen $mac"
        found=$(inventory | awk -v m="$mac" '$1 == m')
        if [ -n "$found" ]; then
            read -r _ hub port tty <<<"$found"
            state="present"
        else
            # Absent from the bus. The stored location is what is left, and it
            # is the reason this can still cut power to a wedged board.
            read -r hub port < <(board_location "$mac") || true
            tty="-"
            if [ -n "${hub:-}" ]; then
                state="not plugged in (location from devices.list)"
            else
                hub="-"; port="-"
                state="not plugged in, and no location recorded"
            fi
        fi
        board_forbidden "$mac" && state="REFUSED: $(board_forbidden_why "$mac")"
        printf "%-12s %-20s %-8s %-6s %-10s %s\n" "$name" "$mac" "$hub" "$port" "$tty" "$state"
    done < <(grep -vE '^[[:space:]]*(#|$)' "$list")

    while read -r serial hub port tty; do
        [[ " $seen " == *" ${serial^^} "* ]] && continue
        printf "%-12s %-20s %-8s %-6s %-10s %s\n" "?" "$serial" "$hub" "$port" "$tty" \
            "not in devices.list"
    done < <(inventory | sort)
}

cmd_cycle() {
    local mac
    mac=$(resolve "${1:?usage: power.sh cycle <mac|role>}")

    # **Checked by MAC, from the file, so it holds while the board is absent.**
    # A flag keyed to a port or a ttyACM number protects a location; this
    # protects a device, which is the thing that must not be cut.
    if board_forbidden "$mac"; then
        die "refusing $mac ($(board_name_of "$mac")) -- $(board_forbidden_why "$mac")"
    fi
    [ -n "$(board_name_of "$mac")" ] || die "$mac is not in devices.list -- add it first"

    local found hub port tty source
    found=$(inventory | awk -v m="$mac" '$1 == m')
    if [ -n "$found" ]; then
        read -r _ hub port tty <<<"$found"
        source="sysfs"
    else
        # **The case this exists for.** A board that has wedged or gone to sleep
        # with its USB PHY down is not in sysfs at all, and that is precisely
        # when its power needs cutting. The stored location is the only thing
        # left to act on.
        read -r hub port < <(board_location "$mac") || true
        tty="-"
        [ -n "${hub:-}" ] || die "$mac is not plugged in and devices.list records no hub and port for it.
Add them as the third and fourth columns -- \`tools/power.sh list\` prints the
location of every board that IS visible, which is how to find them while it is."
        source="devices.list"
    fi

    [ "$hub" = "-" ] && die "$mac is on a root port -- no switchable hub above it"

    echo "power: cycling $mac ($(board_name_of "$mac")) -- hub $hub port $port, $tty"
    if [ "$source" = "devices.list" ]; then
        # Cutting a port on trust: say what is actually on it, if anything, so a
        # board that has been replugged elsewhere cannot be cut by proxy.
        local occupant
        occupant=$(inventory | awk -v h="$hub" -v p="$port" '$2 == h && $3 == p {print $1}')
        echo "power: the board is absent -- location from devices.list, not the bus."
        if [ -n "$occupant" ]; then
            echo "power: ⚠ that port currently holds $occupant ($(board_name_of "$occupant"))."
            echo "power:   the board has moved, or devices.list is wrong. Refusing."
            exit 1
        fi
        echo "power: nothing is enumerating there, which is consistent."
    fi
    sudo -n uhubctl -f -l "$hub" -p "$port" -a cycle -d "${OFF_SECONDS:-4}" >/dev/null

    # Wait for it to come back rather than assuming. Re-enumeration takes a
    # couple of seconds and the caller almost always wants to talk to it next.
    #
    # `awk` rather than the obvious `grep -q`: grep exits the moment it matches,
    # which closes the pipe, kills `inventory` with SIGPIPE, and -- under the
    # `pipefail` set at the top of this file -- makes the pipeline report 141.
    # A successful match would look like a failure, so the loop could only ever
    # end by timing out. awk reads its input to the end, so it cannot happen.
    local waited=0 found_now=""
    while [ -z "$found_now" ]; do
        found_now=$(inventory | awk -v m="$mac" '$1 == m')
        [ -n "$found_now" ] && break
        [ $((waited++)) -gt 300 ] && die "$mac did not come back after the cycle"
        sleep 0.1
    done
    read -r _ _ _ tty <<<"$found_now"
    echo "power: back as $tty after $((waited / 10))s"
}

case "${1:-list}" in
    list)  cmd_list ;;
    cycle) shift; cmd_cycle "$@" ;;
    *)     die "usage: power.sh [list | cycle <mac|role>]" ;;
esac
