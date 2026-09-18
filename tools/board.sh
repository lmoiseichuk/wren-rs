# Which boards this bench has, and which of them a script may touch.
#
# **The MACs are not in the scripts, and not in the repository.** They are this
# bench's identities; the scripts are the part worth sharing. `devices.list` is
# gitignored, `devices.list.example` shows the format, and a clone is somebody
# else's bench rather than a copy of this one.
#
# Sourced by `tools/power.sh`. Ported from the lightning project, which arrived
# at the same problem from the other end -- three scripts each carrying a
# hardcoded MAC.
#
# Where a MAC comes from, first match wins:
#
#   1. `$BOARD_MAC` in the environment    -- for a one-off or for CI
#   2. the entry named `$BOARD`, or the name passed to `board_mac`
#
# Format: `name  MAC  [hub  port]  [flags]`.
#
# **Hub and port are the fallback that makes a missing board reachable.** While
# a board is plugged in, sysfs is the better answer -- it is live, so it follows
# a replug. But a board that has wedged, gone to sleep with its USB PHY down, or
# stopped enumerating for any other reason is *absent from sysfs entirely*, and
# that is exactly when its power needs cutting. Without a stored location there
# is nothing left to cut.

# The devices file, or failure if there is none.
devices_list() {
    local here dir
    here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    for dir in "$here" "$here/.." "$here/../.."; do
        if [[ -f "$dir/devices.list" ]]; then
            printf '%s\n' "$dir/devices.list"
            return 0
        fi
    done
    return 1
}

# The MAC for a name, uppercased. With no name, the first entry.
board_mac() {
    if [[ -n "${BOARD_MAC:-}" ]]; then
        printf '%s\n' "${BOARD_MAC^^}"
        return 0
    fi
    local want="${1:-${BOARD:-}}" list
    list="$(devices_list)" || return 1
    awk -v want="${want,,}" '
        /^[[:space:]]*(#|$)/ { next }
        { name = tolower($1) }
        want != "" && name == want { print toupper($2); found = 1; exit }
        want == "" && !first { first = toupper($2) }
        END { if (!found && want == "" && first) print first }
    ' "$list"
}

# Which name a MAC belongs to, so a message can say more than the hex.
board_name_of() {
    local mac="${1^^}" list
    list="$(devices_list)" || return 1
    awk -v mac="$mac" '
        /^[[:space:]]*(#|$)/ { next }
        toupper($2) == mac { print $1; exit }
    ' "$list"
}

# The stored `hub port` for a name or MAC, if the entry has one.
#
# The fallback for a board sysfs cannot see. Empty when the entry does not say,
# which is the honest answer for a board on a root port or one whose location
# was never recorded.
board_location() {
    local want="$1" list
    list="$(devices_list)" || return 1
    awk -v want="${want,,}" '
        /^[[:space:]]*(#|$)/ { next }
        (tolower($1) == want || toupper($2) == toupper(want)) {
            # hub is like 5-1; port is the bare number after it
            for (i = 3; i < NF; i++) {
                if ($i ~ /^[0-9]+-[0-9.]+$/ && $(i+1) ~ /^[0-9]+$/) {
                    print $i, $(i+1)
                    exit
                }
            }
            exit 1
        }
    ' "$list"
}

# Every flag on the entry owning `$1` (a MAC), space separated.
#
# Flags are any bare words after the MAC that are not a hub/port pair, so the
# columns stay optional without needing placeholders.
board_flags() {
    local mac="${1^^}" list
    list="$(devices_list)" || return 1
    awk -v mac="$mac" '
        /^[[:space:]]*(#|$)/ { next }
        toupper($2) != mac { next }
        {
            out = ""
            for (i = 3; i <= NF; i++) {
                if ($i ~ /^#/) break
                # A hub is like 5-1 or 1-1.2; a port is a bare number.
                if ($i ~ /^[0-9]+-[0-9.]+$/ || $i ~ /^[0-9]+$/) continue
                out = out (out == "" ? "" : " ") $i
            }
            print out
            exit
        }
    ' "$list"
}

# True when this MAC is flagged `forbidden` -- a board this project must never
# write to or power-cycle, wherever it happens to be plugged in.
#
# **The flag travels with the board, not with the port.** Port numbers and
# ttyACM<n> both move; the MAC is printed on the chip. A guard keyed to anything
# else is one replug away from protecting the wrong device.
board_forbidden() {
    [[ " $(board_flags "$1") " == *" forbidden "* ]]
}

# Why it is forbidden: the rest of the line after a `#`, if the entry says.
board_forbidden_why() {
    local mac="${1^^}" list why
    list="$(devices_list)" || return 1
    why="$(awk -v mac="$mac" '
        /^[[:space:]]*(#|$)/ { next }
        toupper($2) == mac && index($0, "#") { print substr($0, index($0, "#") + 1); exit }
    ' "$list")"
    printf '%s\n' "${why:-flagged forbidden in devices.list}" | sed 's/^ *//'
}

# Explain how to configure one, when a script needs the list and has none.
board_list_missing() {
    cat >&2 <<'EOF'
No devices.list, so this cannot tell one board from another.

Every script that can write to or power-cycle a device looks the board up there
first -- including the one board on this bench that must never be touched.
Create it:

    cp devices.list.example devices.list
    $EDITOR devices.list

Find the MACs with:

    tools/power.sh list
EOF
}
