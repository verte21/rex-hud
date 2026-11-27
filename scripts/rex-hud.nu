#!/usr/bin/env nu

def resolve-rex-hud-binary [] {
    let explicit = ($env | get -o REX_HUD_BIN | default "")
    if not ($explicit | is-empty) and ($explicit | path exists) {
        return $explicit
    }

    let script_dir = ($env.FILE_PWD? | default $env.PWD)
    let local_bin = ($env.HOME | path join ".local/bin/rex-hud")
    let candidates = if ($nu.os-info.name == "linux") {
        [
            $local_bin
            ($script_dir | path join "rex-hud-linux-x86_64")
            ($script_dir | path join "rex-hud")
            "/home/linuxbrew/.linuxbrew/bin/rex-hud"
            "/usr/local/bin/rex-hud"
        ]
    } else {
        [
            $local_bin
            ($script_dir | path join "rex-hud")
            "/opt/homebrew/bin/rex-hud"
            "/usr/local/bin/rex-hud"
        ]
    }

    for candidate in $candidates {
        if ($candidate | path exists) {
            return $candidate
        }
    }

    let in_path = (which rex-hud)
    if not ($in_path | is-empty) {
        return $in_path.0.path
    }

    null
}

def main [
    --cwd: string = ""
    --scale: string = ""
    --text
    --snapshot
    --install: string = ""
] {
    if not ($install | is-empty) {
        let source = ($install | path expand)
        if not ($source | path exists) {
            error make { msg: $"Binary not found: ($source)" }
        }
        let target_dir = ($env.HOME | path join ".local/bin")
        let target_bin = ($target_dir | path join "rex-hud")
        mkdir $target_dir
        cp -f $source $target_bin
        ^chmod +x $target_bin
        print $"Installed rex-hud to ($target_bin)"
        return
    }

    let binary = (resolve-rex-hud-binary)
    if $binary == null {
        error make {
            msg: "rex-hud binary not found. Copy rex-hud-linux-x86_64 to ~/.local/bin/rex-hud or run: nu rex-hud.nu --install ./rex-hud-linux-x86_64"
        }
    }

    if $snapshot {
        ^$binary snapshot
        return
    }

    mut args = ["ui" "--mode" "kiosk"]
    if not ($cwd | is-empty) {
        $args = ($args | append ["--cwd" ($cwd | path expand)])
    }
    if not ($scale | is-empty) {
        $args = ($args | append ["--scale" $scale])
    }
    if $text {
        $args = ($args | append "--text")
    }

    let final_args = $args
    if $text {
        with-env { REX_HUD_TEXT: "1" } {
            ^$binary ...$final_args
        }
    } else {
        with-env { REX_HUD_KITTY: "1" } {
            ^$binary ...$final_args
        }
    }
}
