//! Классификатор безопасных read-only shell-команд по модели `codex-rs/shell-command`.

const MAX_UNWRAP_DEPTH: usize = 4;

/// Проверяет, состоит ли shell-команда (включая пайпы, цепочки и обёртки `sh -c` / `bash -lc`)
/// исключительно из известных безопасных read-only команд без перенаправлений и подстановок.
pub fn is_known_safe_command(command: &str) -> bool {
    is_known_safe_command_with_depth(command, 0)
}

fn is_known_safe_command_with_depth(command: &str, depth: usize) -> bool {
    if depth > MAX_UNWRAP_DEPTH {
        return false;
    }

    let Some(commands) = split_shell_commands(command) else {
        return false;
    };
    if commands.is_empty() {
        return false;
    }

    commands.iter().all(|tokens| {
        if let Some(inner) = unwrap_shell_invocation(tokens) {
            is_known_safe_command_with_depth(inner, depth + 1)
        } else {
            is_single_command_safe(tokens)
        }
    })
}

/// Разбивает shell-строку на отдельные команды и аргументы, отвергая опасные конструкции
/// (`$(...)`, обратные кавычки, перенаправления `>` / `<`, одиночный `&`, незакрытые кавычки).
pub fn split_shell_commands(input: &str) -> Option<Vec<Vec<String>>> {
    let chars: Vec<char> = input.chars().collect();
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut current_cmd: Vec<String> = Vec::new();
    let mut current_arg = String::new();
    let mut has_arg = false;

    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    let flush_arg =
        |has_arg: &mut bool, current_arg: &mut String, current_cmd: &mut Vec<String>| {
            if *has_arg {
                current_cmd.push(std::mem::take(current_arg));
                *has_arg = false;
            }
        };

    while i < chars.len() {
        let ch = chars[i];

        if in_single {
            if ch == '\'' {
                in_single = false;
            } else {
                current_arg.push(ch);
            }
            i += 1;
            continue;
        }

        if in_double {
            match ch {
                '"' => {
                    in_double = false;
                }
                '\\' => {
                    i += 1;
                    if i >= chars.len() {
                        return None;
                    }
                    current_arg.push(chars[i]);
                }
                '`' => return None,
                '$' if chars.get(i + 1) == Some(&'(') => return None,
                _ => current_arg.push(ch),
            }
            i += 1;
            continue;
        }

        match ch {
            '\'' => {
                in_single = true;
                has_arg = true;
            }
            '"' => {
                in_double = true;
                has_arg = true;
            }
            '\\' => {
                i += 1;
                if i >= chars.len() {
                    return None;
                }
                has_arg = true;
                current_arg.push(chars[i]);
            }
            '`' | '>' | '<' => return None,
            '$' if chars.get(i + 1) == Some(&'(') => return None,
            ';' | '\n' | '\r' => {
                flush_arg(&mut has_arg, &mut current_arg, &mut current_cmd);
                if !current_cmd.is_empty() {
                    commands.push(std::mem::take(&mut current_cmd));
                }
            }
            '|' => {
                flush_arg(&mut has_arg, &mut current_arg, &mut current_cmd);
                if current_cmd.is_empty() {
                    return None;
                }
                commands.push(std::mem::take(&mut current_cmd));
                if chars.get(i + 1) == Some(&'|') {
                    i += 1;
                }
            }
            '&' => {
                if chars.get(i + 1) == Some(&'&') {
                    flush_arg(&mut has_arg, &mut current_arg, &mut current_cmd);
                    if current_cmd.is_empty() {
                        return None;
                    }
                    commands.push(std::mem::take(&mut current_cmd));
                    i += 1;
                } else {
                    // Фоновый запуск `&` не считаем безопасным для авто-одобрения
                    return None;
                }
            }
            c if c.is_whitespace() => {
                flush_arg(&mut has_arg, &mut current_arg, &mut current_cmd);
            }
            c => {
                has_arg = true;
                current_arg.push(c);
            }
        }

        i += 1;
    }

    if in_single || in_double {
        return None;
    }

    flush_arg(&mut has_arg, &mut current_arg, &mut current_cmd);
    if !current_cmd.is_empty() {
        commands.push(current_cmd);
    }

    Some(commands)
}

fn unwrap_shell_invocation(tokens: &[String]) -> Option<&str> {
    let program = tokens.first()?.as_str();
    if !matches!(program, "sh" | "bash" | "zsh" | "dash") {
        return None;
    }
    if tokens.len() == 3 && matches!(tokens[1].as_str(), "-c" | "-lc") {
        return Some(tokens[2].as_str());
    }
    None
}

fn is_unsafe_path_value(val: &str) -> bool {
    if val.is_empty() {
        return false;
    }
    if val.starts_with('/') || val.starts_with('\\') || val.starts_with('~') {
        return true;
    }
    let bytes = val.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return true;
    }
    val == ".." || val.split(['/', '\\']).any(|seg| seg == "..")
}

fn has_unsafe_path_arg(args: &[&str]) -> bool {
    for arg in args {
        if let Some((_, val)) = arg.split_once('=')
            && is_unsafe_path_value(val)
        {
            return true;
        }
        if is_unsafe_path_value(arg) {
            return true;
        }
        if arg.starts_with('-')
            && !arg.starts_with("--")
            && arg.len() > 2
            && is_unsafe_path_value(&arg[2..])
        {
            return true;
        }
    }
    false
}

fn is_single_command_safe(tokens: &[String]) -> bool {
    let Some(program) = tokens.first().map(String::as_str) else {
        return false;
    };
    // Запрещаем относительные и абсолютные пути вроде `./sed` или `/tmp/ls`,
    // чтобы локальный бинарь не мог замаскироваться под системную команду.
    if program.contains('/') || program.contains('\\') {
        return false;
    }

    let args: Vec<&str> = tokens[1..].iter().map(String::as_str).collect();

    match program {
        "pwd" | "which" | "whereis" | "type" | "basename" | "dirname" | "echo" | "printf"
        | "true" | "false" | "uname" | "hostname" | "uptime" | "whoami" | "id" | "groups"
        | "date" | "cal" | "df" | "du" | "free" | "vmstat" | "iostat" | "lscpu" | "lsblk"
        | "lsmem" | "lspci" | "lsusb" | "ps" | "pgrep" | "ss" | "netstat" | "nproc" | "sensors"
        | "acpi" | "findmnt" | "hostnamectl" => true,

        "ls" | "ll" | "la" | "dir" | "cat" | "head" | "tail" | "wc" | "nl" | "cut" | "sort"
        | "uniq" | "tr" | "grep" | "egrep" | "fgrep" | "rg" | "ag" | "file" | "stat"
        | "readlink" | "realpath" => !has_unsafe_path_arg(&args),

        "sed" => {
            !has_unsafe_path_arg(&args)
                && !args.iter().any(|arg| {
                    *arg == "-i" || arg.starts_with("-i") || arg.starts_with("--in-place")
                })
        }

        "find" => {
            !has_unsafe_path_arg(&args)
                && !args.iter().any(|arg| {
                    matches!(
                        *arg,
                        "-exec"
                            | "-execdir"
                            | "-ok"
                            | "-okdir"
                            | "-delete"
                            | "-fprint"
                            | "-fprintf"
                            | "-fls"
                    )
                })
        }

        "top" => args.iter().any(|arg| *arg == "-b" || *arg == "--batch"),

        "git" => is_safe_git(&args),

        "cargo" => matches!(
            args.first().copied(),
            Some("metadata" | "tree" | "version" | "--version")
        ),

        "systemctl" => matches!(
            args.first().copied(),
            Some("status" | "is-active" | "is-enabled" | "list-units" | "list-unit-files")
        ),

        "journalctl" => !args.iter().any(|arg| {
            arg.starts_with("--vacuum")
                || matches!(*arg, "--rotate" | "--flush" | "--sync" | "--relinquish-var")
        }),

        "sysctl" => !args
            .iter()
            .any(|arg| *arg == "-w" || *arg == "--write" || arg.contains('=')),

        "ip" => {
            matches!(
                args.first().copied(),
                Some("addr" | "address" | "route" | "link" | "neigh" | "rule" | "maddr")
            ) && !args.iter().any(|arg| {
                matches!(
                    *arg,
                    "add" | "del" | "delete" | "set" | "change" | "replace" | "flush"
                )
            })
        }

        "swapon" => args.contains(&"--show") || args.is_empty(),

        "mount" => args.is_empty() || args == ["-l"],

        "timedatectl" => args.is_empty() || args.first().copied() == Some("status"),

        _ => false,
    }
}

fn is_safe_git(args: &[&str]) -> bool {
    if args.is_empty() {
        return true;
    }
    if args
        .iter()
        .any(|arg| *arg == "-c" || arg.starts_with("--ext-diff") || arg.starts_with("--exec"))
    {
        return false;
    }
    match args[0] {
        "status" | "diff" | "log" | "show" | "ls-files" | "ls-tree" | "describe" | "rev-parse" => {
            true
        }
        "branch" => !args.iter().any(|arg| {
            matches!(
                *arg,
                "-d" | "-D" | "-m" | "-M" | "-c" | "-C" | "--delete" | "--move" | "--copy"
            )
        }),
        "tag" => !args
            .iter()
            .any(|arg| matches!(*arg, "-d" | "--delete" | "-a" | "-s" | "-f")),
        "remote" => args.len() == 1 || args[1..] == ["-v"] || args[1..] == ["show"],
        "config" => args
            .iter()
            .any(|arg| matches!(*arg, "--get" | "--get-all" | "--list" | "-l")),
        _ => false,
    }
}
