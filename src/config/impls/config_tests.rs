#[cfg(test)]
mod tests {

    /// 老配置里的客户端授权没有新加的两字段：反序列化必须当成"从未授权、
    /// 未信任"，而不是报错或被误判成已信任。
    #[test]
    fn legacy_client_grant_deserialises_as_unsigned() {
        let grant: McpClientGrant =
            serde_json::from_str(r#"{"id":"Claude Desktop 0.7.2","permissions":["read"]}"#)
                .expect("legacy grant must deserialize");
        assert_eq!(grant.last_auth_unix, 0);
        assert!(!grant.trusted);
        assert_eq!(grant.permissions, vec!["read".to_string()]);
    }
    use super::super::*;

    fn temp_store() -> ConfigStore {
        let dir = std::env::temp_dir();
        let id = Uuid::new_v4();
        ConfigStore {
            path: dir.join(format!("ms-test-{id}-sessions.json")),
            settings_path: dir.join(format!("ms-test-{id}-settings.json")),
            backup_dir: None,
            cache: ConfigFile::default(),
            pending: false,
            key: [7u8; 32],
        }
    }

    /// `write_atomic` 落盘的文件必须是 0600。
    ///
    /// 老写法先按 umask（通常 0644）建文件、之后再 chmod，中间那段窗口里
    /// `sessions.json.tmp` 是**世界可读**的 —— 里面是（加密的）凭据。
    #[cfg(unix)]
    #[test]
    fn write_atomic_publishes_owner_only_files() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("ms-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sessions.json");
        // 放一个旧版本留下的 0644 文件：新写法在覆盖它之后也得是 0600。
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        ConfigStore::write_atomic(&path, "{}").unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "sessions.json 应为 0600，实际 {mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 新生成的 `secret.key` 同样必须从创建那一刻就是 0600，并且带 fsync。
    #[cfg(unix)]
    #[test]
    fn generated_secret_key_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("ms-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        ConfigStore::load_or_create_key(&dir).unwrap();

        let key_path = dir.join("secret.key");
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "secret.key 应为 0600，实际 {mode:o}");
        assert_eq!(std::fs::metadata(&key_path).unwrap().len(), 32);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 私有目录的判据是"真目录 + 0700 + 属主是自己"。
    ///
    /// 别的本地账户在 `/tmp` 里抢先摆一个同名目录（或 0700 的符号链接）骗不过去。
    #[cfg(unix)]
    #[test]
    fn private_dir_check_rejects_world_readable_and_symlinks() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("ms-test-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let good = dir.join("good");
        std::fs::create_dir(&good).unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(is_private_dir(&good));

        let loose = dir.join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!is_private_dir(&loose), "0755 的目录不该算私有目录");

        let link = dir.join("link");
        std::os::unix::fs::symlink(&good, &link).unwrap();
        assert!(!is_private_dir(&link), "符号链接不该算私有目录");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `create_private_temp_dir`：随机名、0700、创建即独占。
    #[cfg(unix)]
    #[test]
    fn create_private_temp_dir_is_random_and_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let a = create_private_temp_dir("ms-test").unwrap();
        let b = create_private_temp_dir("ms-test").unwrap();
        assert_ne!(a, b, "两次调用应当拿到不同的目录名");
        for dir in [&a, &b] {
            let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "{} 应为 0700，实际 {mode:o}", dir.display());
        }
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// 写盘失败只记日志，**不能 panic** —— 这正是 `save_logging` / `flush_logging`
    /// 存在的意义（此前是 `let _ = s.save();` 静默吞掉）。
    #[test]
    fn logging_saves_survive_an_unwritable_path() {
        let mut store = temp_store();
        // 把路径指到一个**目录**上：`write_atomic` 必然失败。
        store.path = std::env::temp_dir();
        store.save_logging();
        store.flush_logging();
    }

    /// 命令历史里的口令不该明文落在 `sessions.json` 里，读回来要能还原。
    #[test]
    fn command_history_is_encrypted_at_rest() {
        let mut store = temp_store();
        store.push_command_history("mysql -psecret123".to_string());
        store.save().unwrap();

        let raw = std::fs::read_to_string(&store.path).unwrap();
        assert!(!raw.contains("secret123"), "命令历史不该明文落盘：{raw}");

        // 模拟下次启动：读回来解密，应当还原原文。
        let file: SessionsFile = serde_json::from_str(&raw).unwrap();
        assert!(
            file.command_history[0].starts_with(ConfigStore::ENC_PREFIX),
            "落盘的应当是密文：{}",
            file.command_history[0]
        );
        let mut cfg = ConfigFile {
            command_history: file.command_history.clone(),
            ..Default::default()
        };
        ConfigStore::decrypt_into(&mut cfg, &store.key);
        assert_eq!(cfg.command_history, vec!["mysql -psecret123".to_string()]);

        let _ = std::fs::remove_file(&store.path);
        let _ = std::fs::remove_file(&store.settings_path);
    }

    /// 旧文件里的明文命令历史仍要读得进来 —— 别因为新增加密就把历史清空。
    #[test]
    fn legacy_plaintext_command_history_still_loads() {
        let mut cfg = ConfigFile {
            command_history: vec!["ls -la".to_string()],
            ..Default::default()
        };
        ConfigStore::decrypt_into(&mut cfg, &[7u8; 32]);
        assert_eq!(cfg.command_history, vec!["ls -la".to_string()]);
    }

    #[test]
    fn imports_finalshell_export_and_reencrypts_password_at_rest() {
        let mut store = temp_store();
        let raw = r#"{
            "conection_type": 100,
            "name": "FinalShell host",
            "host": "192.0.2.20",
            "port": 22,
            "user_name": "operator",
            "password": "AwcLDRETFx1OXQgZJNatCplesw+x/P04",
            "authentication_type": 1,
            "terminal_encoding": "UTF-8"
        }"#;

        assert_eq!(store.import_json(raw).unwrap(), (1, 0));
        let session = &store.cache.sessions[0];
        assert_eq!(session.host, "192.0.2.20");
        assert_eq!(session.user, "operator");
        assert_eq!(session.password.as_str(), "meatshell-test");

        let saved = std::fs::read_to_string(&store.path).unwrap();
        assert!(!saved.contains("meatshell-test"));
        let disk: ConfigFile = serde_json::from_str(&saved).unwrap();
        assert!(
            disk.sessions[0]
                .password
                .as_str()
                .starts_with(ConfigStore::ENC_PREFIX)
        );

        let _ = std::fs::remove_file(&store.path);
    }

    #[test]
    fn terminal_cursor_style_defaults_and_validates() {
        let mut store = temp_store();
        // 出厂默认 = 竖线（与终端页「还原本页默认」一致）。
        assert_eq!(store.terminal_cursor_style(), "bar");

        store.set_terminal_cursor_style("block".into());
        assert_eq!(store.terminal_cursor_style(), "block");
        store.set_terminal_cursor_style("underline".into());
        assert_eq!(store.terminal_cursor_style(), "underline");
        // 非法值回落到出厂默认，而不是另一个硬编码的形状。
        store.set_terminal_cursor_style("unexpected".into());
        assert_eq!(store.terminal_cursor_style(), "bar");

        store.cache = serde_json::from_str("{}").expect("legacy config must deserialize");
        assert_eq!(store.terminal_cursor_style(), "bar");
    }

    /// 设置页那三档语义值是平台无关的：旧后端名与 macOS 写进配置的 `femtovg-wgpu`
    /// 都要归到 GPU 档，认不出的值回"自动"（重新探测一次）。
    #[test]
    fn renderer_mode_choice_normalizes_legacy_and_backend_names() {
        let mut store = temp_store();
        assert_eq!(store.renderer_mode_choice(), "auto");

        store.set_renderer_mode_choice("software");
        assert_eq!(store.renderer_mode_choice(), "software");
        store.set_renderer_mode_choice("wgpu");
        assert_eq!(store.renderer_mode_choice(), "wgpu");
        store.set_renderer_mode_choice("auto");
        assert_eq!(store.renderer_mode_choice(), "auto");

        // 前置条件直写 cache，不能走分平台的 set_renderer_mode —— 那会把旧值就地归一化，
        // 验不到读取端。
        for legacy in ["gpu", "skia", "skia-vulkan", "femtovg", "femtovg-wgpu"] {
            store.cache.appearance.renderer_mode = legacy.into();
            assert_eq!(
                store.renderer_mode_choice(),
                "wgpu",
                "{legacy} 应归到 GPU 档"
            );
        }
        store.cache.appearance.renderer_mode = "unexpected".into();
        assert_eq!(store.renderer_mode_choice(), "auto");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn renderer_mode_defaults_to_auto_and_validates() {
        let mut store = temp_store();
        // 默认 `auto`：首次启动探测一次，结论写回配置（见 app/window.rs）。
        assert_eq!(store.renderer_mode(), "auto");

        store.set_renderer_mode("wgpu".into());
        assert_eq!(store.renderer_mode(), "wgpu");
        store.set_renderer_mode("software".into());
        assert_eq!(store.renderer_mode(), "software");
        // 旧配置里的 `gpu`（OpenGL）升到 wgpu 档 —— OpenGL 已从矩阵退役。
        store.set_renderer_mode("gpu".into());
        assert_eq!(store.renderer_mode(), "wgpu");
        // 瞎值回到"重新探测一次"，而不是默默压成软件渲染。
        store.set_renderer_mode("unexpected".into());
        assert_eq!(store.renderer_mode(), "auto");

        store.cache = serde_json::from_str("{}").expect("legacy config must deserialize");
        assert_eq!(store.renderer_mode(), "auto");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn renderer_mode_preserves_linux_automatic_default_and_validates() {
        let mut store = temp_store();
        assert_eq!(store.renderer_mode(), "auto");

        store.set_renderer_mode("wgpu".into());
        assert_eq!(store.renderer_mode(), "wgpu");
        store.set_renderer_mode("software".into());
        assert_eq!(store.renderer_mode(), "software");
        // 旧值 `gpu`（OpenGL）与支线早期的 `skia-vulkan` 都升到 wgpu 档。
        store.set_renderer_mode("gpu".into());
        assert_eq!(store.renderer_mode(), "wgpu");
        store.set_renderer_mode("skia-vulkan".into());
        assert_eq!(store.renderer_mode(), "wgpu");
        store.set_renderer_mode("unexpected".into());
        assert_eq!(store.renderer_mode(), "auto");

        store.cache = serde_json::from_str("{}").expect("legacy config must deserialize");
        assert_eq!(store.renderer_mode(), "auto");
    }

    #[test]
    fn quick_connect_groups_default_collapsed_and_remember_expansion() {
        let mut store = temp_store();
        store.cache.groups = vec!["production".into(), "staging".into()];
        store.cache.sessions.push(Session {
            group: "production".into(),
            ..sample_session("server")
        });

        assert!(store.collapsed_session_groups().is_none());
        store.set_session_group_collapsed("production", false);

        let collapsed = store.collapsed_session_groups().unwrap();
        assert!(!collapsed.iter().any(|group| group == "production"));
        assert!(collapsed.iter().any(|group| group == "staging"));
        assert!(collapsed.iter().any(|group| group == "system"));

        store.set_session_group_collapsed("production", true);
        assert!(
            store
                .collapsed_session_groups()
                .unwrap()
                .iter()
                .any(|group| group == "production")
        );
    }

    // ── A 方案：回滚上限治理 ────────────────────────────────────────────────

    /// 旧配置里存着超过常规上限的行数时，迁移必须**自动打开**大回滚开关 ——
    /// 否则新的 10 万上限会在下次写入时把用户原有的回滚静默截断（A.5 的风险项）。
    #[test]
    fn large_scrollback_migration_keeps_old_oversized_limits() {
        let mut cfg = ConfigFile {
            defaults_rev: 5,
            ..ConfigFile::default()
        };
        cfg.terminal.scrollback_lines = 500_000;
        cfg.terminal.large_scrollback = false;

        assert!(migrate_defaults(&mut cfg));
        assert!(
            cfg.terminal.large_scrollback,
            "超限的旧配置必须自动打开开关"
        );
        assert_eq!(cfg.terminal.scrollback_lines, 500_000, "原上限不能被改动");

        // 常规范围内的配置不受影响
        let mut cfg2 = ConfigFile {
            defaults_rev: 5,
            ..ConfigFile::default()
        };
        cfg2.terminal.scrollback_lines = 50_000;
        migrate_defaults(&mut cfg2);
        assert!(!cfg2.terminal.large_scrollback);
        assert_eq!(cfg2.terminal.scrollback_lines, 50_000);
    }

    /// 上界随开关变化；关掉开关会把现值收回常规上限（不留"上限 10 万却存着 50 万"）。
    #[test]
    fn scrollback_clamp_follows_the_large_buffer_switch() {
        let mut store = temp_store();

        store.set_scrollback_lines(500_000);
        assert_eq!(
            store.scrollback_lines(),
            SCROLLBACK_MAX,
            "开关关着时被常规上限夹住"
        );

        store.set_large_scrollback(true);
        store.set_scrollback_lines(500_000);
        assert_eq!(store.scrollback_lines(), 500_000, "开关打开后放到 100 万");

        store.set_large_scrollback(false);
        assert_eq!(
            store.scrollback_lines(),
            SCROLLBACK_MAX,
            "关闭开关必须把现值收回常规上限"
        );
        assert_eq!(store.scrollback_lines(), 100_000);
    }

    #[test]
    fn issue_316_reserved_system_groups_are_repaired_and_rejected() {
        let mut system_session = sample_session("misfiled");
        system_session.group = "system".into();
        system_session.password = Secret::default();
        let mut default_session = sample_session("legacy-default");
        default_session.group = "Default".into();
        let mut cfg = ConfigFile {
            sessions: vec![system_session, default_session],
            groups: vec![
                "system".into(),
                "System".into(),
                "default".into(),
                "prod".into(),
            ],
            collapsed_session_groups: Some(vec!["system".into(), "prod".into()]),
            ..ConfigFile::default()
        };

        assert!(normalize_reserved_session_groups(&mut cfg));
        assert_eq!(cfg.groups, ["prod"]);
        assert!(cfg.sessions.iter().all(|session| session.group.is_empty()));
        assert!(cfg.sessions[0].password.is_empty());
        // The built-in system folder's collapse preference is display state,
        // not a user-created group, so normalization must preserve it.
        assert_eq!(
            cfg.collapsed_session_groups.as_deref(),
            Some(["system".to_string(), "prod".to_string()].as_slice())
        );

        let mut store = temp_store();
        store.add_group("system".into());
        store.add_group("DEFAULT".into());
        store.add_group("prod".into());
        store.rename_group("prod", "System".into());
        assert_eq!(store.groups(), ["prod"]);

        let mut session = sample_session("server");
        session.group = "SYSTEM".into();
        let id = session.id.clone();
        store.upsert(session);
        assert_eq!(store.get(&id).unwrap().group, "");
    }

    #[test]
    fn session_group_names_are_unique_case_insensitively() {
        let mut store = temp_store();
        store.add_group("Production".into());
        store.add_group("production".into());
        assert_eq!(store.groups(), ["Production"]);
        assert!(store.session_group_exists(" PRODUCTION "));

        let mut session = sample_session("staging-server");
        session.group = "Staging".into();
        store.upsert(session);
        assert!(store.session_group_exists("staging"));

        store.rename_group("Production", "STAGING".into());
        assert_eq!(store.groups(), ["Production"]);

        // Changing only the spelling/case of the same group remains valid.
        store.rename_group("Production", "production".into());
        assert_eq!(store.groups(), ["production"]);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn renderer_mode_uses_macos_backends_and_validates() {
        let mut store = temp_store();
        // 默认档 = GPU（FemtoVG on wgpu → Metal）；OpenGL 与 Skia 两档都已退役。
        assert_eq!(store.renderer_mode(), "femtovg-wgpu");

        store.set_renderer_mode("software".into());
        assert_eq!(store.renderer_mode(), "software");
        store.set_renderer_mode("femtovg-wgpu".into());
        assert_eq!(store.renderer_mode(), "femtovg-wgpu");
        // 退役的旧值（Skia / OpenGL 版 FemtoVG）统一升到 wgpu 档，不留在已下架的渲染器上。
        store.set_renderer_mode("skia".into());
        assert_eq!(store.renderer_mode(), "femtovg-wgpu");
        store.set_renderer_mode("femtovg".into());
        assert_eq!(store.renderer_mode(), "femtovg-wgpu");
        store.set_renderer_mode("unexpected".into());
        assert_eq!(store.renderer_mode(), "femtovg-wgpu");

        store.cache = serde_json::from_str("{}").expect("legacy config must deserialize");
        assert_eq!(store.renderer_mode(), "femtovg-wgpu");
    }

    /// 旧配置里写死的出厂默认 `#FFFFFF` 按**跟随主题**处理 —— 它是默认值，
    /// 不是"用户显式挑了白色"（浅色主题下的白光标几乎看不见）。
    #[test]
    fn legacy_white_cursor_follows_the_theme() {
        let mut store = temp_store();
        store.set_terminal_cursor_color("#FFFFFF");
        assert_eq!(store.terminal_cursor_color(), "");
    }

    #[test]
    fn terminal_cursor_color_normalizes_and_rejects_invalid_values() {
        let mut store = temp_store();
        // 出厂默认 = 空串（**跟随主题**：深色档亮色 / 浅色档暗色）。历史上曾改成写死的
        // #FFFFFF，代价是浅色主题下光标几乎看不见 —— 用户要求改回跟随（见 CHANGELOG）。
        assert_eq!(store.terminal_cursor_color(), "");
        // 清空 = 回到"跟随主题"（空串是一个**选项**，不是非法值）。
        assert!(store.set_terminal_cursor_color("   "));
        assert_eq!(store.terminal_cursor_color(), "");

        assert!(store.set_terminal_cursor_color("#1a2B3c"));
        assert_eq!(store.terminal_cursor_color(), "#1A2B3C");
        assert!(store.set_terminal_cursor_color("abcdef"));
        assert_eq!(store.terminal_cursor_color(), "#ABCDEF");

        assert!(!store.set_terminal_cursor_color("#12345"));
        assert_eq!(store.terminal_cursor_color(), "#ABCDEF");
        assert!(!store.set_terminal_cursor_color("#GG0000"));
        assert_eq!(store.terminal_cursor_color(), "#ABCDEF");
    }

    fn sample_session(name: &str) -> Session {
        Session {
            name: name.into(),
            host: "192.168.100.2".into(),
            port: 22,
            user: "root".into(),
            ..Session::new_empty()
        }
    }

    #[test]
    fn restores_and_syncs_user_config_backup() {
        let base = std::env::temp_dir().join(format!("ms-backup-{}", Uuid::new_v4()));
        let primary = base.join("portable");
        let backup = base.join("user");
        std::fs::create_dir_all(&primary).unwrap();
        std::fs::create_dir_all(&backup).unwrap();

        let backup_cfg = ConfigFile {
            sessions: vec![sample_session("saved")],
            ..ConfigFile::default()
        };
        std::fs::write(
            backup.join("sessions.json"),
            serde_json::to_string_pretty(&backup_cfg).unwrap(),
        )
        .unwrap();
        std::fs::write(backup.join("secret.key"), [9u8; 32]).unwrap();

        restore_user_backup_if_needed(&primary, &backup);
        assert!(sessions_file_has_connections(
            &primary.join("sessions.json")
        ));
        assert_eq!(
            std::fs::read(primary.join("secret.key")).unwrap(),
            [9u8; 32]
        );

        let store = ConfigStore {
            path: primary.join("sessions.json"),
            settings_path: primary.join("settings.json"),
            backup_dir: Some(backup.clone()),
            cache: ConfigFile {
                sessions: vec![sample_session("new")],
                ..ConfigFile::default()
            },
            pending: false,
            key: [7u8; 32],
        };
        std::fs::write(primary.join("secret.key"), [7u8; 32]).unwrap();
        store.save().unwrap();

        let raw = std::fs::read_to_string(backup.join("sessions.json")).unwrap();
        let cfg: ConfigFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(cfg.sessions.len(), 1);
        assert_eq!(cfg.sessions[0].name, "new");
        assert_eq!(std::fs::read(backup.join("secret.key")).unwrap(), [7u8; 32]);

        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn wallpaper_defaults_to_ms_but_keeps_explicit_choice() {
        // Fresh install (no file).
        let fresh = fresh_config();
        assert_eq!(fresh.appearance.wallpaper, "builtin:dark");
        assert!(
            (fresh.appearance.wallpaper_overlay - DEFAULT_WALLPAPER_OVERLAY).abs() < f32::EPSILON
        );
        // User upgrading from before the feature: JSON without the key.
        let cfg: ConfigFile = serde_json::from_str("{}").unwrap();
        assert_eq!(cfg.appearance.wallpaper, "builtin:dark");
        // An explicit "无"/none (stored as "") is preserved, not re-defaulted.
        let cfg: ConfigFile = serde_json::from_str(r#"{"wallpaper":""}"#).unwrap();
        assert_eq!(cfg.appearance.wallpaper, "");
        // A custom choice is preserved.
        let cfg: ConfigFile = serde_json::from_str(r#"{"wallpaper":"builtin:light"}"#).unwrap();
        assert_eq!(cfg.appearance.wallpaper, "builtin:light");

        // Retired built-ins (tech/miku/ms) migrate to the dark wallpaper once.
        let mut cfg = ConfigFile {
            appearance: AppearanceSettings {
                wallpaper: "builtin:miku".to_string(),
                ..Default::default()
            },
            defaults_rev: DEFAULTS_REV - 1,
            ..ConfigFile::default()
        };
        assert!(migrate_defaults(&mut cfg));
        assert_eq!(cfg.appearance.wallpaper, "builtin:dark");
    }

    #[test]
    fn wallpaper_transparency_default_migrates_without_overwriting_custom_value() {
        let mut old_default = ConfigFile {
            appearance: AppearanceSettings {
                wallpaper_overlay: PREVIOUS_DEFAULT_WALLPAPER_OVERLAY,
                ..Default::default()
            },
            defaults_rev: 2,
            ..ConfigFile::default()
        };
        assert!(migrate_defaults(&mut old_default));
        assert!(
            (old_default.appearance.wallpaper_overlay - DEFAULT_WALLPAPER_OVERLAY).abs()
                < f32::EPSILON
        );

        // 自定义滑条值不被迁移覆盖（取一个不与新默认重合的值）。
        let mut custom = ConfigFile {
            appearance: AppearanceSettings {
                wallpaper_overlay: 0.95,
                ..Default::default()
            },
            defaults_rev: 2,
            ..ConfigFile::default()
        };
        assert!(migrate_defaults(&mut custom));
        assert!((custom.appearance.wallpaper_overlay - 0.95).abs() < f32::EPSILON);
    }

    // ── rev 7：内嵌 Meatshell Mono 移除后的字体配置迁移 ──────────────────────

    fn font_cfg(rev: u32, term: &str, ui: &str) -> ConfigFile {
        let mut cfg = ConfigFile {
            defaults_rev: rev,
            ..ConfigFile::default()
        };
        cfg.terminal.font_family = term.to_string();
        cfg.appearance.ui_font_family = ui.to_string();
        cfg
    }

    #[test]
    fn font_migration_rewrites_only_exact_meatshell_values() {
        let mut cfg = font_cfg(6, "Meatshell Mono", "Meatshell Mono");
        assert!(migrate_defaults(&mut cfg));
        assert_eq!(cfg.terminal.font_family.as_str(), "JetBrains Mono");
        assert!(cfg.appearance.ui_font_family.is_empty());
        assert_eq!(cfg.defaults_rev, DEFAULTS_REV);
    }

    #[test]
    fn font_migration_preserves_other_user_choices() {
        let mut cfg = font_cfg(6, "Maple Mono NF CN", "Helvetica Neue");
        assert!(migrate_defaults(&mut cfg));
        assert_eq!(cfg.terminal.font_family.as_str(), "Maple Mono NF CN");
        assert_eq!(cfg.appearance.ui_font_family.as_str(), "Helvetica Neue");
    }

    #[test]
    fn font_migration_runs_once_and_never_reverts_later_choices() {
        let mut cfg = font_cfg(6, "Meatshell Mono", "Meatshell Mono");
        assert!(migrate_defaults(&mut cfg));
        cfg.terminal.font_family = "Meatshell Mono".to_string();
        assert!(!migrate_defaults(&mut cfg));
        assert_eq!(cfg.terminal.font_family.as_str(), "Meatshell Mono");
    }

    #[test]
    fn font_migration_ignores_near_miss_values() {
        for value in ["Meatshell Mono Extra", " meatshell mono", "Meatshell Mono "] {
            let mut cfg = font_cfg(6, value, value);
            assert!(migrate_defaults(&mut cfg));
            assert_eq!(cfg.terminal.font_family.as_str(), value);
            assert_eq!(cfg.appearance.ui_font_family.as_str(), value);
        }
    }

    #[test]
    fn rev6_upgrade_leaves_other_settings_untouched() {
        let mut cfg = font_cfg(6, "JetBrains Mono", "");
        cfg.appearance.wallpaper_overlay = 0.95;
        cfg.terminal.scrollback_lines = 50_000;
        assert!(migrate_defaults(&mut cfg));
        assert!((cfg.appearance.wallpaper_overlay - 0.95).abs() < f32::EPSILON);
        assert_eq!(cfg.terminal.scrollback_lines, 50_000);
        assert!(!cfg.terminal.large_scrollback);
    }

    #[test]
    fn fresh_config_skips_font_migration() {
        let mut cfg = fresh_config();
        assert_eq!(cfg.defaults_rev, DEFAULTS_REV);
        assert!(!migrate_defaults(&mut cfg));
        assert_eq!(cfg.terminal.font_family.as_str(), "JetBrains Mono");
        assert!(cfg.appearance.ui_font_family.is_empty());
    }

    #[test]
    fn output_highlight_defaults_and_preset_validation() {
        let mut store = temp_store();
        assert!(store.output_highlight_enabled());
        assert_eq!(store.output_highlight_preset(), "builtin");

        store.set_output_highlight_enabled(false);
        store.set_output_highlight_preset("devops".to_string());
        assert!(!store.output_highlight_enabled());
        assert_eq!(store.output_highlight_preset(), "devops");

        store.set_output_highlight_preset("future-preset".to_string());
        assert_eq!(store.output_highlight_preset(), "builtin");

        store.add_output_highlight_rule(OutputHighlightRule {
            pattern: "  connection refused  ".to_string(),
            regex: false,
            case_sensitive: false,
            whole_line: true,
            color: "unknown".to_string(),
            enabled: true,
        });
        assert_eq!(store.output_highlight_rules().len(), 1);
        assert_eq!(
            store.output_highlight_rules()[0].pattern,
            "connection refused"
        );
        assert_eq!(store.output_highlight_rules()[0].color, "red");
        store.set_output_highlight_rule_enabled(0, false);
        assert!(!store.output_highlight_rules()[0].enabled);
        store.remove_output_highlight_rule(0);
        assert!(store.output_highlight_rules().is_empty());

        // An older settings file without either field retains the feature that
        // shipped in the previous version: enabled with the Rudder built-in
        // (tailspin-style) preset.
        let legacy: ConfigFile = serde_json::from_str("{}").unwrap();
        store.cache = legacy;
        assert!(store.output_highlight_enabled());
        assert_eq!(store.output_highlight_preset(), "builtin");
    }

    #[test]
    fn saved_password_encrypts_and_decrypts_without_changes() {
        let mut store = temp_store();
        let password = "p@ss word!^&*中文";
        store.cache.sessions.push(Session {
            name: "windows-password".into(),
            host: "192.168.100.2".into(),
            port: 22,
            user: "root".into(),
            password: Secret::new(password),
            ..Session::new_empty()
        });

        store.save().unwrap();
        let raw = std::fs::read_to_string(&store.path).unwrap();
        assert!(!raw.contains(password));
        let disk: ConfigFile = serde_json::from_str(&raw).unwrap();
        let encrypted = disk.sessions[0].password.as_str();
        assert!(encrypted.starts_with(ConfigStore::ENC_PREFIX));
        assert_eq!(
            ConfigStore::try_decrypt(&store.key, encrypted).as_deref(),
            Some(password)
        );

        let _ = std::fs::remove_file(&store.path);
    }

    #[test]
    fn proxy_passwords_are_encrypted_at_rest() {
        // 代理密码是明文网络凭据：写盘前必须与登录密码同一套加密，
        // 否则 `sessions.json` 里就躺着一份可直接拿去用代理的口令。
        let mut store = temp_store();
        let secret = "proxy-s3cret-hunter2";
        store.cache.sessions.push(Session {
            name: "via-proxy".into(),
            host: "10.1.2.3".into(),
            proxy: "socks5://127.0.0.1:1080".into(),
            proxy_user: "bob".into(),
            proxy_pass: Secret::new(secret),
            ..Session::new_empty()
        });

        store.save().unwrap();
        let raw = std::fs::read_to_string(&store.path).unwrap();
        assert!(!raw.contains(secret), "proxy password written in the clear");
        let disk: ConfigFile = serde_json::from_str(&raw).unwrap();
        assert_eq!(disk.sessions[0].proxy_user, "bob");
        let encrypted = disk.sessions[0].proxy_pass.as_str();
        assert!(encrypted.starts_with(ConfigStore::ENC_PREFIX));
        assert_eq!(
            ConfigStore::try_decrypt(&store.key, encrypted).as_deref(),
            Some(secret)
        );

        let _ = std::fs::remove_file(&store.path);
    }

    #[test]
    fn trigger_responses_are_encrypted_at_rest() {
        // `SessionTrigger::response` documents itself as "stored encrypted like
        // the session password" — the usual way a user fills it in is with the
        // password that answers a `Password:` prompt, so a regression here
        // silently writes a credential in the clear.
        let mut store = temp_store();
        let answer = "trigger-secret-hunter2";
        store.cache.sessions.push(Session {
            name: "prompt-answer".into(),
            host: "10.0.0.9".into(),
            triggers: vec![SessionTrigger {
                expect: "Password:".into(),
                response: Secret::new(answer),
                ..SessionTrigger::default()
            }],
            ..Session::new_empty()
        });

        store.save().unwrap();
        let raw = std::fs::read_to_string(&store.path).unwrap();
        assert!(!raw.contains(answer));
        let disk: ConfigFile = serde_json::from_str(&raw).unwrap();
        let encrypted = disk.sessions[0].triggers[0].response.as_str();
        assert!(encrypted.starts_with(ConfigStore::ENC_PREFIX));
        assert_eq!(
            ConfigStore::try_decrypt(&store.key, encrypted).as_deref(),
            Some(answer)
        );

        let _ = std::fs::remove_file(&store.path);
    }

    #[test]
    fn legacy_plaintext_trigger_response_is_encrypted_on_next_save() {
        // Sessions written before trigger responses were encrypted hold a bare
        // string. `load()` leaves it alone (try_decrypt returns None) so it
        // reaches save() verbatim — which must then encrypt it, exactly like a
        // legacy plaintext password.
        let mut store = temp_store();
        store.cache.sessions.push(Session {
            name: "legacy".into(),
            host: "10.0.0.10".into(),
            triggers: vec![SessionTrigger {
                expect: "yes/no".into(),
                response: Secret::new("legacy-plaintext"),
                ..SessionTrigger::default()
            }],
            ..Session::new_empty()
        });
        assert!(ConfigStore::try_decrypt(&store.key, "legacy-plaintext").is_none());

        store.save().unwrap();
        let raw = std::fs::read_to_string(&store.path).unwrap();
        assert!(!raw.contains("legacy-plaintext"));
        let disk: ConfigFile = serde_json::from_str(&raw).unwrap();
        assert!(
            disk.sessions[0].triggers[0]
                .response
                .as_str()
                .starts_with(ConfigStore::ENC_PREFIX)
        );

        let _ = std::fs::remove_file(&store.path);
    }

    #[test]
    fn a_wrong_sized_key_file_is_kept_aside_not_overwritten() {
        // `secret.key` is the only thing that can decrypt the stored passwords,
        // so a wrong-sized blob must survive rather than being replaced in place.
        let dir = std::env::temp_dir().join(format!("ms-key-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_path = dir.join("secret.key");
        std::fs::write(&key_path, b"too-short").unwrap();

        let key = ConfigStore::load_or_create_key(&dir).unwrap();

        // A fresh 32-byte key is generated and installed...
        assert_eq!(key.len(), 32);
        assert_eq!(std::fs::read(&key_path).unwrap().len(), 32);
        // ...and the unusable original is preserved for recovery.
        assert_eq!(
            std::fs::read(dir.join("secret.key.bad")).unwrap(),
            b"too-short"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_blob_from_a_different_key_is_detectable_as_undecryptable() {
        // This is the condition `load()` counts to log its error: the value is
        // still an `enc:v1:` blob, but the current key cannot open it.
        let blob = ConfigStore::encrypt(&[9u8; 32], "hunter2").unwrap();
        assert!(blob.starts_with(ConfigStore::ENC_PREFIX));
        assert!(ConfigStore::try_decrypt(&[7u8; 32], &blob).is_none());
        // With the right key it round-trips, so the negative above is the key
        // mismatch and not a broken blob.
        assert_eq!(
            ConfigStore::try_decrypt(&[9u8; 32], &blob).as_deref(),
            Some("hunter2")
        );
    }

    #[test]
    fn export_import_roundtrip_preserves_password() {
        let mut a = temp_store();
        a.cache.sessions.push(Session {
            name: "pve".into(),
            host: "192.168.100.2".into(),
            port: 22,
            user: "root".into(),
            password: Secret::new("s3cr3t"),
            ..Session::new_empty()
        });

        let export_path = std::env::temp_dir().join(format!("ms-exp-{}.json", Uuid::new_v4()));
        assert_eq!(a.export_to(&export_path).unwrap(), 1);

        // The file keeps host/user plaintext but the password is obfuscated.
        let raw = std::fs::read_to_string(&export_path).unwrap();
        assert!(raw.contains("192.168.100.2"));
        assert!(raw.contains(ConfigStore::EXPORT_PREFIX));
        assert!(!raw.contains("s3cr3t"));

        // Importing into a fresh store recovers the plaintext password.
        let mut b = temp_store();
        assert_eq!(b.import_from(&export_path).unwrap(), (1, 0));
        assert_eq!(b.cache.sessions.len(), 1);
        assert_eq!(b.cache.sessions[0].password.as_str(), "s3cr3t");
        assert_eq!(b.cache.sessions[0].host, "192.168.100.2");

        // Re-importing the same file skips the duplicate.
        assert_eq!(b.import_from(&export_path).unwrap(), (0, 1));

        let _ = std::fs::remove_file(&export_path);
        let _ = std::fs::remove_file(&a.path);
        let _ = std::fs::remove_file(&b.path);
    }

    // ── Drag-to-reorder (D6) ─────────────────────────────────────────────

    /// Display order for these tests: default(d1, d2), alpha(a1), beta(b1, b2)
    /// — "default" first, named groups alphabetically, stored Vec order
    /// matching display order.
    fn reorder_store() -> ConfigStore {
        let mut store = temp_store();
        let mk = |name: &str, group: &str| Session {
            id: format!("id-{name}"),
            name: name.into(),
            group: group.into(),
            ..Session::new_empty()
        };
        store.cache.sessions = vec![
            mk("d1", ""),
            mk("d2", ""),
            mk("a1", "alpha"),
            mk("b1", "beta"),
            mk("b2", "beta"),
        ];
        // Empty collapse list = every group expanded.
        store.cache.collapsed_session_groups = Some(Vec::new());
        store
    }

    fn id_of(store: &ConfigStore, name: &str) -> String {
        store
            .sessions()
            .iter()
            .find(|s| s.name == name)
            .expect("session present")
            .id
            .clone()
    }

    fn order_of(store: &ConfigStore) -> Vec<(String, String)> {
        store
            .sessions()
            .iter()
            .map(|s| (s.name.clone(), s.group.clone()))
            .collect()
    }

    #[test]
    fn reorder_session_swaps_same_group_neighbours() {
        let mut store = reorder_store();
        assert!(store.reorder_session(&id_of(&store, "d1"), 1));
        assert_eq!(
            order_of(&store)[..2],
            [("d2".into(), "".into()), ("d1".into(), "".into())]
        );
    }

    #[test]
    fn reorder_session_at_group_boundary_moves_into_the_neighbour_group() {
        let mut store = reorder_store();
        // Last member of "default" dragging down → joins alpha at its top
        // (first member moving down lands at the target group's head).
        assert!(store.reorder_session(&id_of(&store, "d2"), 1));
        assert_eq!(
            order_of(&store),
            [
                ("d1".into(), "".into()),
                ("d2".into(), "alpha".into()),
                ("a1".into(), "alpha".into()),
                ("b1".into(), "beta".into()),
                ("b2".into(), "beta".into()),
            ]
        );
    }

    #[test]
    fn collapsed_groups_are_skipped_when_crossing() {
        let mut store = reorder_store();
        store.cache.collapsed_session_groups = Some(vec!["alpha".into()]);
        // d2 dragging down must skip the collapsed alpha and land in beta.
        assert!(store.reorder_session(&id_of(&store, "d2"), 1));
        assert_eq!(
            order_of(&store),
            [
                ("d1".into(), "".into()),
                ("a1".into(), "alpha".into()),
                ("d2".into(), "beta".into()),
                ("b1".into(), "beta".into()),
                ("b2".into(), "beta".into()),
            ]
        );
    }

    #[test]
    fn emptying_a_named_group_keeps_it_as_an_empty_folder() {
        let mut store = reorder_store();
        // a1 is alpha's only member; moving it up lands at default's bottom
        // (last member moving up joins the previous group's tail).
        assert!(store.reorder_session(&id_of(&store, "a1"), -1));
        let groups = store.groups().to_vec();
        assert!(groups.iter().any(|g| g == "alpha"), "alpha folder survives");
        assert_eq!(
            order_of(&store),
            [
                ("d1".into(), "".into()),
                ("d2".into(), "".into()),
                ("a1".into(), "".into()),
                ("b1".into(), "beta".into()),
                ("b2".into(), "beta".into()),
            ]
        );
    }

    #[test]
    fn unknown_id_and_unknown_group_are_no_ops() {
        let mut store = reorder_store();
        assert!(!store.reorder_session("nope", 1));
        // A lone session in an otherwise-empty display list cannot hop.
        let mut lone = temp_store();
        lone.cache.sessions = vec![Session {
            id: "only".into(),
            name: "only".into(),
            group: String::new(),
            ..Session::new_empty()
        }];
        lone.cache.collapsed_session_groups = Some(Vec::new());
        assert!(!lone.reorder_session("only", 1));
        assert!(!lone.reorder_session("only", -1));
    }

    // ── Zen mode (focus) ─────────────────────────────────────────────────

    #[test]
    fn zen_mode_defaults_off_and_round_trips() {
        let mut store = temp_store();
        assert!(!store.zen_mode());

        store.set_zen_mode(true);
        assert!(store.zen_mode());

        // Persisted form stays compatible with configs written before zen.
        let raw = serde_json::to_string(&store.cache).unwrap();
        let back: ConfigFile = serde_json::from_str(&raw).unwrap();
        assert!(back.layout.zen_mode);
    }

    #[test]
    fn legacy_config_without_zen_field_still_loads() {
        let raw = r#"{"download_always_ask": true}"#;
        let cache: ConfigFile = serde_json::from_str(raw).unwrap();
        assert!(!cache.layout.zen_mode);
    }

    // ── Data-dir migration (#141) ────────────────────────────────────────
    //
    // `migrate_legacy` is the one-shot copy that runs the first time a build
    // lands on a new data dir. It is the only code that moves user secrets
    // between directories, so its three guarantees are asserted here:
    // copy everything, never clobber an existing destination, leave the
    // originals in place.

    #[test]
    fn migrate_legacy_copies_all_three_data_files() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        for name in ["sessions.json", "secret.key", "known_hosts"] {
            std::fs::write(src.path().join(name), format!("content:{name}")).unwrap();
        }

        migrate_legacy(src.path(), dst.path());

        for name in ["sessions.json", "secret.key", "known_hosts"] {
            assert_eq!(
                std::fs::read_to_string(dst.path().join(name)).unwrap(),
                format!("content:{name}"),
                "{name} must be migrated"
            );
        }
    }

    #[test]
    fn migrate_legacy_never_overwrites_an_existing_destination() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("sessions.json"), b"from-legacy").unwrap();
        std::fs::write(dst.path().join("sessions.json"), b"already-there").unwrap();

        migrate_legacy(src.path(), dst.path());

        assert_eq!(
            std::fs::read_to_string(dst.path().join("sessions.json")).unwrap(),
            "already-there",
            "an existing destination file must survive migration"
        );
    }

    /// 旧版 MSI 的数据在 `%TEMP%\rudder`：升级后第一次以可写的 `config` 启动时
    /// 必须整套搬过来，否则用户的会话与设置全空。
    #[test]
    fn migrate_temp_legacy_imports_the_temp_config() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        for name in [
            "sessions.json",
            "secret.key",
            "settings.json",
            "known_hosts",
        ] {
            std::fs::write(src.path().join(name), format!("content:{name}")).unwrap();
        }

        migrate_temp_legacy(src.path(), dst.path());

        for name in [
            "sessions.json",
            "secret.key",
            "settings.json",
            "known_hosts",
        ] {
            assert_eq!(
                std::fs::read_to_string(dst.path().join(name)).unwrap(),
                format!("content:{name}"),
                "{name} must be migrated"
            );
        }
    }

    /// 目标目录已经在用（哪怕只有密钥）就不搬：临时目录里的旧会话可能是用户
    /// 主动清掉的，掺回来比丢数据更糟。
    #[test]
    fn migrate_temp_legacy_skips_a_config_already_in_use() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("sessions.json"), b"stale").unwrap();
        std::fs::write(src.path().join("secret.key"), b"stale-key").unwrap();
        std::fs::write(dst.path().join("secret.key"), b"current-key").unwrap();

        migrate_temp_legacy(src.path(), dst.path());

        assert!(
            !dst.path().join("sessions.json").exists(),
            "在用的配置不该被掺入旧会话"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("secret.key")).unwrap(),
            "current-key"
        );
    }

    /// 目标已经有会话时不动它，也不动它对应的 key。
    #[test]
    fn migrate_temp_legacy_keeps_existing_sessions_and_key() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("sessions.json"), b"stale").unwrap();
        std::fs::write(src.path().join("secret.key"), b"stale-key").unwrap();
        std::fs::write(dst.path().join("sessions.json"), b"current").unwrap();
        std::fs::write(dst.path().join("secret.key"), b"current-key").unwrap();

        migrate_temp_legacy(src.path(), dst.path());

        assert_eq!(
            std::fs::read_to_string(dst.path().join("sessions.json")).unwrap(),
            "current"
        );
        assert_eq!(
            std::fs::read_to_string(dst.path().join("secret.key")).unwrap(),
            "current-key"
        );
    }

    #[test]
    fn migrate_legacy_leaves_the_source_files_in_place() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("secret.key"), b"original").unwrap();

        migrate_legacy(src.path(), dst.path());

        assert_eq!(
            std::fs::read_to_string(src.path().join("secret.key")).unwrap(),
            "original",
            "migration copies, never moves"
        );
    }

    #[test]
    fn migrate_legacy_ignores_missing_source_files() {
        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        std::fs::write(src.path().join("known_hosts"), b"only-this-one").unwrap();

        migrate_legacy(src.path(), dst.path());

        assert!(dst.path().join("known_hosts").exists());
        assert!(!dst.path().join("sessions.json").exists());
        assert!(!dst.path().join("secret.key").exists());
    }

    #[test]
    fn migrate_legacy_is_a_noop_for_the_same_directory() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("sessions.json"), b"untouched").unwrap();

        migrate_legacy(dir.path(), dir.path());

        assert_eq!(
            std::fs::read_to_string(dir.path().join("sessions.json")).unwrap(),
            "untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn migrate_legacy_makes_the_secret_key_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let src = tempfile::tempdir().unwrap();
        let dst = tempfile::tempdir().unwrap();
        let key = src.path().join("secret.key");
        std::fs::write(&key, b"secret-bytes").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();

        migrate_legacy(src.path(), dst.path());

        let mode = std::fs::metadata(dst.path().join("secret.key"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "secret.key must be 0600, got {mode:o}");
    }

    #[test]
    fn dedup_sorted_sorts_case_insensitively_and_drops_exact_duplicates() {
        // Stable sort by lowercase; exact-case duplicates are adjacent after
        // the sort so `dedup()` removes them. Case-variants ("zeta" vs "ZETA")
        // are NOT merged — this matches the pre-refactor behaviour this helper
        // was extracted from (4 call sites did sort_by_key(lowercase)+dedup).
        assert_eq!(
            dedup_sorted(vec![
                "zeta".into(),
                "Alpha".into(),
                "beta".into(),
                "alpha".into(),
                "ZETA".into(),
            ]),
            vec!["Alpha", "alpha", "beta", "zeta", "ZETA"]
        );
        assert!(dedup_sorted(Vec::new()).is_empty());

        // Exact duplicates are removed regardless of case-grouping.
        assert_eq!(
            dedup_sorted(vec!["SSH".into(), "ssh".into(), "ftp".into(), "ssh".into()]),
            vec!["ftp", "SSH", "ssh"]
        );
    }

    /// macOS keeps every user file under one per-user directory: logs are a
    /// subdir of the data dir (#macos-data-dir).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_log_dir_is_a_subdir_of_the_data_dir() {
        let data = data_dir();
        let log = log_dir();
        assert!(
            log.starts_with(&data),
            "log dir {log:?} must live under data dir {data:?}"
        );
        assert_eq!(log.file_name().unwrap(), "log");
        assert!(log.is_dir(), "log_dir() must create its directory");
    }

    /// The macOS branch must resolve to the per-user OS config dir — the
    /// app bundle (/Applications) is shared and read-only, so no `config/`
    /// folder may ever be created beside the executable.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_data_dir_is_the_per_user_os_dir_and_not_exe_adjacent() {
        let data = data_dir();
        assert_eq!(
            data,
            legacy_data_dir().expect("per-user OS config dir"),
            "macOS must use the official per-user location"
        );
        if let Some(portable) = portable_data_dir() {
            assert_ne!(
                data, portable,
                "macOS must not use the exe-adjacent portable dir"
            );
        }
    }
}

#[cfg(test)]
mod domain_split_compat_tests {
    use super::super::*;

    /// 分域改造前后 on-disk 格式必须完全一致：仍是**平铺** key-value。
    ///
    /// 这是整个重构的成败点 —— `#[serde(flatten)]` 一旦不生效，用户现有的
    /// sessions.json 会整体读不出来（等于丢配置）。所以这里同时验两件事：
    /// 老格式能读进来、写出去还是老格式。
    #[test]
    fn flat_json_round_trips_through_the_domain_structs() {
        // 一份"改造前"形态的配置：所有设置都平铺在顶层。
        let legacy = r##"{
            "font_family": "Maple Mono",
            "font_size": 15,
            "terminal_bold": true,
            "scrollback_lines": 12000,
            "convert_eol": false,
            "osc52_clipboard": false,
            "terminal_cursor_style": "underline",
            "terminal_cursor_color": "#1A2B3C",
            "output_highlight_preset": "devops",
            "ui_scale": 120,
            "panel_font": 90,
            "wallpaper": "builtin:light",
            "wallpaper_overlay": 0.8,
            "hide_special_partitions": false,
            "mount_filter": "/data /home",
            "renderer_mode": "skia",
            "sftp_no_follow_cd": true,
            "download_always_ask": true,
            "download_dir": "/tmp/dl",
            "collapse_sidebar_default": true,
            "welcome_as_sidebar": true,
            "quick_commands_as_sidebar": true,
            "sidebar_dock": "left",
            "welcome_sidebar_width": 321.0,
            "window_width": 1440.0,
            "sync_upload": true,
            "webdav_url": "https://dav.example/x",
            "webdav_username": "bob",
            "update_check_disabled": true,
            "animations_disabled": true,
            "defaults_rev": 5
        }"##;
        // `sidebar_dock` / `welcome_as_sidebar` / `welcome_sidebar_width` 都是已移除的
        // 字段：老配置里仍有它们，读取必须当未知键忽略而不是报错。
        let cfg: ConfigFile = serde_json::from_str(legacy).expect("老的平铺配置必须仍能读入");

        // 每个域都取到了自己的字段。
        assert_eq!(cfg.terminal.font_family, "Maple Mono");
        assert_eq!(cfg.terminal.font_size, 15);
        assert_eq!(cfg.terminal.scrollback_lines, 12000);
        assert!(!cfg.terminal.convert_eol);
        assert_eq!(cfg.terminal.terminal_cursor_style, "underline");
        assert_eq!(cfg.appearance.ui_scale, 120);
        assert_eq!(cfg.appearance.wallpaper, "builtin:light");
        assert_eq!(cfg.appearance.mount_filter, "/data /home");
        assert_eq!(cfg.appearance.renderer_mode, "skia");
        assert!(cfg.appearance.animations_disabled);
        assert!(cfg.transfer.sftp_no_follow_cd);
        assert_eq!(cfg.transfer.download_dir, "/tmp/dl");
        assert_eq!(cfg.layout.window_width, 1440.0);
        assert_eq!(cfg.sync.webdav_url, "https://dav.example/x");
        assert!(cfg.update.update_check_disabled);

        // ── 写出去必须还是平铺格式（不能出现 "terminal": {...} 这种嵌套）──
        let out = serde_json::to_string(&cfg).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let obj = v.as_object().expect("顶层必须是对象");
        for nested in [
            "terminal",
            "appearance",
            "layout",
            "transfer",
            "sync",
            "update",
        ] {
            assert!(
                !obj.contains_key(nested),
                "分域结构体被序列化成了嵌套对象 {nested:?} —— flatten 失效，用户配置会读不出来"
            );
        }
        for key in [
            "font_family",
            "scrollback_lines",
            "ui_scale",
            "wallpaper_overlay",
            "download_dir",
            "webdav_url",
            "update_check_disabled",
        ] {
            assert!(obj.contains_key(key), "平铺键 {key:?} 丢失");
        }
        assert_eq!(obj["font_family"], "Maple Mono");
        assert_eq!(obj["window_width"], 1440.0);
    }

    /// 三处默认值来源合一：serde 缺字段、`ConfigFile::default()`、各域 `Default`。
    ///
    /// 改造前它们是三套互不一致的答案（serde fn / 派生 Default / getter 哨兵），
    /// 「还原本页默认」取错了一处就还原成白纸 —— 这是实际发生过的 bug。
    ///
    /// 逐字段比较而非 `assert_eq!` 整个结构体：`SyncSettings` 含 `Secret`，
    /// 不应为了测试方便给口令类型派发 `PartialEq`。
    #[test]
    fn all_three_default_sources_agree() {
        let from_missing: ConfigFile = serde_json::from_str("{}").unwrap();
        let from_default = ConfigFile::default();
        let fresh = fresh_config();

        let d = TerminalSettings::default();
        let a = AppearanceSettings::default();
        let l = LayoutSettings::default();

        for cfg in [&from_missing, &from_default, &fresh] {
            // 终端域：这几项是"曾经还原成白纸"的重灾区，必须逐个钉住。
            assert_eq!(cfg.terminal.font_family, d.font_family);
            assert_eq!(cfg.terminal.font_size, d.font_size);
            assert_eq!(cfg.terminal.scrollback_lines, d.scrollback_lines);
            assert_eq!(cfg.terminal.convert_eol, d.convert_eol);
            assert_eq!(cfg.terminal.osc52_clipboard, d.osc52_clipboard);
            assert_eq!(cfg.terminal.terminal_cursor_style, d.terminal_cursor_style);
            assert_eq!(cfg.terminal.terminal_cursor_color, d.terminal_cursor_color);
            assert_eq!(
                cfg.terminal.output_highlight_preset,
                d.output_highlight_preset
            );

            assert_eq!(cfg.appearance.wallpaper, a.wallpaper);
            assert_eq!(cfg.appearance.wallpaper_overlay, a.wallpaper_overlay);
            assert_eq!(cfg.appearance.ui_scale, a.ui_scale);
            assert_eq!(cfg.appearance.panel_font, a.panel_font);
            assert_eq!(
                cfg.appearance.hide_special_partitions,
                a.hide_special_partitions
            );

            assert_eq!(
                cfg.layout.collapse_sidebar_default,
                l.collapse_sidebar_default
            );

            assert!(!cfg.transfer.sftp_no_follow_cd, "存储取反：默认跟随 cd");
            assert!(!cfg.transfer.download_always_ask);
            assert!(cfg.sync.webdav_url.is_empty());
            assert!(!cfg.update.update_check_disabled, "默认开启启动检查");
        }
    }

    /// 还原一页 = 替换该域为默认，且**不影响其它域**。
    #[test]
    fn resetting_one_domain_leaves_the_others_alone() {
        let mut cfg = ConfigFile {
            terminal: TerminalSettings {
                font_family: "Comic Sans".into(),
                ..Default::default()
            },
            appearance: AppearanceSettings {
                ui_scale: 150,
                ..Default::default()
            },
            ..Default::default()
        };

        cfg.terminal = TerminalSettings::default();

        assert_eq!(cfg.terminal.font_family, "JetBrains Mono");
        assert_eq!(cfg.appearance.ui_scale, 150, "还原终端页不得动外观页");
    }
}

#[cfg(test)]
mod session_trigger_tests {
    use super::super::*;

    #[test]
    fn legacy_sessions_without_triggers_still_load() {
        // Sessions saved before #212 have no `triggers` key; the serde default
        // must keep them loadable instead of failing the whole config file.
        let raw = r#"{
            "id": "legacy-1",
            "name": "old box",
            "host": "192.0.2.30",
            "port": 22,
            "user": "root",
            "auth": "password",
            "password": ""
        }"#;
        let session: Session = serde_json::from_str(raw).unwrap();
        assert!(session.triggers.is_empty());
    }

    #[test]
    fn triggers_round_trip_with_defaults() {
        let mut session = Session::new_empty();
        session.triggers.push(SessionTrigger {
            expect: "Password:".into(),
            response: Secret::new("hunter2"),
            ..SessionTrigger::default()
        });
        let raw = serde_json::to_string(&session).unwrap();
        let back: Session = serde_json::from_str(&raw).unwrap();

        assert_eq!(back.triggers.len(), 1);
        assert_eq!(back.triggers[0].expect, "Password:");
        assert_eq!(back.triggers[0].response.as_str(), "hunter2");
        // `append_enter` defaults to true, `repeat` to false.
        assert!(back.triggers[0].append_enter);
        assert!(!back.triggers[0].repeat);
    }
}

#[cfg(test)]
mod split_file_tests {
    use super::super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ms-split-{tag}-{}", Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn store_in(dir: &std::path::Path) -> ConfigStore {
        ConfigStore {
            path: dir.join(SESSIONS_FILE),
            settings_path: dir.join(SETTINGS_FILE),
            backup_dir: None,
            cache: ConfigFile::default(),
            pending: false,
            key: [7u8; 32],
        }
    }

    fn session_named(name: &str) -> Session {
        let mut s = Session::new_empty();
        s.name = name.into();
        s
    }

    /// 一份"合并前"形态的老配置：会话与设置挤在同一个文件里。
    const COMBINED: &str = r#"{
        "sessions": [{"id":"legacy-1","name":"old box","host":"192.0.2.30","port":22,"user":"root","auth":"password","password":""}],
        "font_family": "Comic Sans",
        "defaults_rev": 5
    }"#;

    /// 设置落 `settings.json`、会话落 `sessions.json`，互不串门。
    #[test]
    fn settings_and_sessions_are_written_to_separate_files() {
        let dir = temp_dir("two-files");
        let mut store = store_in(&dir);
        store.cache.terminal.font_family = "Maple Mono".into();
        store.cache.sessions.push(session_named("a"));
        store.save().unwrap();

        let settings_raw = fs::read_to_string(dir.join(SETTINGS_FILE)).unwrap();
        let sessions_raw = fs::read_to_string(dir.join(SESSIONS_FILE)).unwrap();

        let settings: SettingsFile = serde_json::from_str(&settings_raw).unwrap();
        let sessions: SessionsFile = serde_json::from_str(&sessions_raw).unwrap();
        assert_eq!(settings.terminal.font_family, "Maple Mono");
        assert_eq!(sessions.sessions.len(), 1);

        // 两个文件都不该带上对方的键 —— 否则等于没拆（改设置还是要重写会话）。
        assert!(
            !settings_raw.contains("\"sessions\""),
            "settings.json 不该含会话数据"
        );
        assert!(
            !sessions_raw.contains("font_family"),
            "sessions.json 不该含设置"
        );
    }

    /// 合并前的老 `sessions.json` 两个视图都读得出来，且各取所需 —— 这就是
    /// "老配置零迁移"的依据：首次载入时把设置那半边另存成 `settings.json`。
    #[test]
    fn legacy_combined_file_is_readable_by_both_views() {
        let sessions: SessionsFile = serde_json::from_str(COMBINED).unwrap();
        let settings: SettingsFile = serde_json::from_str(COMBINED).unwrap();
        assert_eq!(sessions.sessions.len(), 1, "老文件里的会话必须仍在");
        assert_eq!(
            settings.terminal.font_family, "Comic Sans",
            "老文件里的设置也要能读出 —— 首次拆分靠它"
        );
    }

    /// `settings.json` 存在时以它为准，覆盖老 `sessions.json` 里残留的那份设置。
    #[test]
    fn settings_file_wins_over_settings_carried_in_the_sessions_file() {
        let mut cfg: ConfigFile = serde_json::from_str(COMBINED).unwrap();
        assert_eq!(cfg.terminal.font_family, "Comic Sans");
        let settings: SettingsFile =
            serde_json::from_str(r#"{"font_family":"JetBrains Mono","defaults_rev":5}"#).unwrap();
        settings.apply_to(&mut cfg);
        assert_eq!(
            cfg.terminal.font_family, "JetBrains Mono",
            "settings.json 必须覆盖老文件里的设置"
        );
        assert_eq!(cfg.sessions.len(), 1, "覆盖设置不得动会话");
    }

    /// 两个文件的键之并集 == `ConfigFile` 的全部键。
    ///
    /// 防的是"给 ConfigFile 加了字段却忘了同步到落盘视图" —— 那等于这项设置
    /// 永远不会保存，而且没有任何编译错误。
    #[test]
    fn the_two_disk_files_cover_every_config_field() {
        let cfg = ConfigFile::default();
        let whole = serde_json::to_value(&cfg).unwrap();
        let mut got: Vec<String> = Vec::new();
        for value in [
            serde_json::to_value(SessionsFile::from_cache(&cfg)).unwrap(),
            serde_json::to_value(SettingsFile::from_cache(&cfg)).unwrap(),
        ] {
            for key in value.as_object().unwrap().keys() {
                got.push(key.clone());
            }
        }
        got.sort();
        let mut want: Vec<String> = whole.as_object().unwrap().keys().cloned().collect();
        want.sort();
        assert_eq!(got, want, "落盘视图与 ConfigFile 的字段集不一致");
    }

    /// 加密后的口令各回各家：会话口令在 `sessions.json`，WebDAV 口令（sync 域）
    /// 在 `settings.json`；两边都不得出现明文。
    #[test]
    fn secrets_are_encrypted_into_the_file_that_owns_them() {
        let dir = temp_dir("secrets");
        let mut store = store_in(&dir);
        let mut session = session_named("a");
        session.password = Secret::new("pw");
        store.cache.sessions.push(session);
        store.cache.sync.webdav_password = Secret::new("dav");
        store.save().unwrap();

        let sessions_raw = fs::read_to_string(dir.join(SESSIONS_FILE)).unwrap();
        let settings_raw = fs::read_to_string(dir.join(SETTINGS_FILE)).unwrap();
        assert!(sessions_raw.contains(ConfigStore::ENC_PREFIX));
        assert!(!sessions_raw.contains("\"pw\""));
        assert!(settings_raw.contains(ConfigStore::ENC_PREFIX));
        assert!(!settings_raw.contains("\"dav\""));
    }

    /// 防抖语义：挂起期间不落盘，`flush` 才写；没有挂起时 `flush` 是空操作。
    #[test]
    fn debounced_save_only_writes_on_flush() {
        let dir = temp_dir("debounce");
        let mut store = store_in(&dir);
        store.cache.terminal.font_size = 20;
        store.save_debounced();
        assert!(
            !dir.join(SETTINGS_FILE).exists(),
            "挂起期间不应写盘 —— 否则拖动滑条仍是每帧一次"
        );

        store.flush().unwrap();
        assert!(
            fs::read_to_string(dir.join(SETTINGS_FILE))
                .unwrap()
                .contains("20")
        );

        // 再改一次但不标记挂起 → flush 必须什么都不做（退出路径会无条件调用）。
        store.cache.terminal.font_size = 30;
        store.flush().unwrap();
        assert!(
            fs::read_to_string(dir.join(SETTINGS_FILE))
                .unwrap()
                .contains("20"),
            "无挂起时 flush 必须是空操作"
        );
    }

    /// 客户端记忆授权的往返、去重与 32 条上限。
    #[test]
    fn mcp_client_grants_roundtrip_and_cap() {
        let dir = temp_dir("mcp-grants");
        let mut store = store_in(&dir);
        assert!(store.mcp_client_grants().is_empty());

        store.upsert_mcp_client_grant(
            "Claude Desktop 0.7.2",
            vec!["read".to_string(), "exec".to_string()],
            false,
        );
        assert_eq!(
            store
                .mcp_client_grants()
                .iter()
                .find(|grant| grant.id == "Claude Desktop 0.7.2")
                .unwrap()
                .permissions,
            vec!["read", "exec"]
        );

        store.upsert_mcp_client_grant("Client B", vec!["read".to_string()], false);
        store.upsert_mcp_client_grant("Claude Desktop 0.7.2", vec!["read".to_string()], false);
        assert_eq!(store.mcp_client_grants().len(), 2);
        assert_eq!(
            store.mcp_client_grants().last().unwrap().id,
            "Claude Desktop 0.7.2"
        );

        for index in 0..40 {
            store.upsert_mcp_client_grant(&format!("client-{index}"), Vec::new(), false);
        }
        assert_eq!(store.mcp_client_grants().len(), 32);
        assert!(
            !store
                .mcp_client_grants()
                .iter()
                .any(|grant| grant.id == "client-0")
        );

        let before = store.mcp_client_grants().len();
        store.upsert_mcp_client_grant("", Vec::new(), false);
        store.upsert_mcp_client_grant("bad\nid", Vec::new(), false);
        assert_eq!(store.mcp_client_grants().len(), before);

        let _ = fs::remove_dir_all(&dir);
    }

    /// 旧版全局权限键不再被识别，也不会被写回。
    #[test]
    fn legacy_global_permission_keys_are_ignored() {
        let cfg: ConfigFile = serde_json::from_str(
            r#"{"mcp_allow_read":false,"mcp_allow_exec":true,"mcp_allow_host_write":true}"#,
        )
        .unwrap();
        assert!(cfg.mcp.mcp_client_grants.is_empty());
        let settings = serde_json::to_string(&SettingsFile::from_cache(&cfg)).unwrap();
        assert!(!settings.contains("mcp_allow_"), "旧键应被丢弃: {settings}");
    }
}
