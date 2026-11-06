use crate::config::{NotificationMethod, Rule, RuleAction};
use crate::enforcer;
use crate::engine::Engine;
use crate::formatter::{self, MessageContext, TextHorizontalAlignment};
use crate::overlay::OverlayMode;

impl Engine {
    /// Triggers a test overlay for UI verification using Option 3 structured paragraph syntax.
    pub async fn trigger_test_overlay(&mut self, mode: &str) {
        match mode {
            "break" => {
                let text = "# Take a Break\n\n[color=#38bdf8 size=18 bold] Rest your eyes and stretch for {break_time}.";
                let paragraphs = self.parse_rule_message(text, &MessageContext::sample(), TextHorizontalAlignment::Center);
                let mode = OverlayMode::BreakEnforcer {
                    rule_id: "test_break".into(),
                    title: "Test Break Enforcer".into(),
                    paragraphs,
                    duration_seconds: 300,
                    remaining_seconds: 300,
                    allow_snooze: true,
                    strict_mode: false,
                    click_through: false,
                };
                self.show_overlay(mode);
            }
            "hud_toast" => {
                let text = "## App Quota Warning\n[color=#f59e0b bold] You have {remaining} remaining for {app}.";
                let paragraphs = self.parse_rule_message(text, &MessageContext::sample(), TextHorizontalAlignment::Left);
                let mode = OverlayMode::HudToast {
                    rule_id: "test_toast".into(),
                    title: "Quota Warning".into(),
                    paragraphs,
                    snooze_options_minutes: vec![5, 10, 15],
                    duration_seconds: 10,
                    remaining_seconds: 10,
                    show_time_remaining: true,
                };
                self.show_overlay(mode);
            }
            "hud_timer" => {
                let text = "[color=#f87171 bold] Time limit exceeded. Application closing in 30s.";
                let paragraphs = self.parse_rule_message(text, &MessageContext::sample(), TextHorizontalAlignment::Left);
                let mode = OverlayMode::HudTimer {
                    rule_id: "test_timer".into(),
                    title: "Application Closing".into(),
                    paragraphs,
                    duration_seconds: 30,
                    remaining_seconds: 30,
                    allow_snooze: true,
                    strict_mode: false,
                };
                self.show_overlay(mode);
            }
            "hud_status" => {
                self.display_stats_overlay(false).await;
            }
            _ => {}
        }
    }

    /// Triggers an alert preview for a specific rule configuration.
    pub async fn preview_rule_alert(&mut self, rule: &Rule) {
        let ctx = formatter::build_preview_context(rule);

        match rule.action {
            RuleAction::ShowOverlay => {
                let raw_msg = self.get_effective_rule_message(rule);
                let paragraphs =
                    self.parse_rule_message(raw_msg, &ctx, TextHorizontalAlignment::Center);
                let mode = OverlayMode::BreakEnforcer {
                    rule_id: "test_preview".to_string(),
                    title: rule.name.clone(),
                    paragraphs,
                    duration_seconds: 15,
                    remaining_seconds: 15,
                    allow_snooze: true,
                    strict_mode: false,
                    click_through: rule.click_through,
                };
                self.show_overlay(mode);
            }
            RuleAction::ShowNotification => match rule.notification_method {
                NotificationMethod::Windows => {
                    let raw_msg = self.get_effective_rule_message(rule);
                    let text = formatter::format_plain_message(raw_msg, &ctx);
                    enforcer::send_system_notification(&rule.name, &text);
                }
                NotificationMethod::Hud => {
                    let raw_msg = self.get_effective_rule_message(rule);
                    let paragraphs =
                        self.parse_rule_message(raw_msg, &ctx, TextHorizontalAlignment::Left);
                    let dur = rule.notification_duration.to_seconds().max(1);
                    let mode = OverlayMode::HudToast {
                        rule_id: "test_preview".to_string(),
                        title: rule.name.clone(),
                        paragraphs,
                        snooze_options_minutes: if rule.strict_mode {
                            Vec::new()
                        } else {
                            vec![5, 10, 15]
                        },
                        duration_seconds: dur,
                        remaining_seconds: dur,
                        show_time_remaining: rule.show_remaining_time,
                    };
                    self.show_overlay(mode);
                }
            },
            RuleAction::CloseWindow
            | RuleAction::CloseTab
            | RuleAction::KillProcess
            | RuleAction::LockWorkstation => {
                let grace_secs = rule.grace_duration.to_seconds();
                if grace_secs > 0 {
                    let dur = grace_secs.min(15);
                    let default_msg = format!(
                        "[color=#f87171 bold] {} in {}s.",
                        rule.action.as_str(),
                        grace_secs
                    );
                    let raw_msg = if !rule.message.trim().is_empty() {
                        &rule.message
                    } else {
                        &default_msg
                    };
                    let paragraphs =
                        self.parse_rule_message(raw_msg, &ctx, TextHorizontalAlignment::Left);
                    let mode = OverlayMode::HudTimer {
                        rule_id: "test_preview".to_string(),
                        title: format!("{}: {}", rule.action.as_str(), rule.name),
                        paragraphs,
                        duration_seconds: dur,
                        remaining_seconds: dur,
                        allow_snooze: !rule.strict_mode,
                        strict_mode: false,
                    };
                    self.show_overlay(mode);
                } else {
                    let default_msg = format!(
                        "[color=#f59e0b bold] {} in {}.",
                        rule.action.as_str(),
                        &ctx.remaining
                    );
                    let raw_msg = if !rule.message.trim().is_empty() {
                        &rule.message
                    } else {
                        &default_msg
                    };
                    let paragraphs =
                        self.parse_rule_message(raw_msg, &ctx, TextHorizontalAlignment::Left);
                    let dur = rule.notification_duration.to_seconds().max(1);
                    let mode = OverlayMode::HudToast {
                        rule_id: "test_preview".to_string(),
                        title: format!("Upcoming: {}", rule.name),
                        paragraphs,
                        snooze_options_minutes: if rule.strict_mode {
                            Vec::new()
                        } else {
                            vec![5, 10, 15]
                        },
                        duration_seconds: dur,
                        remaining_seconds: dur,
                        show_time_remaining: rule.show_remaining_time,
                    };
                    self.show_overlay(mode);
                }
            }
            RuleAction::ShowStatistics => {
                let grace_secs = rule.grace_duration.to_seconds();
                if grace_secs > 0 {
                    let dur = grace_secs.min(15);
                    let default_msg = format!(
                        "[color=#f87171 bold] {} in {}s.",
                        rule.action.as_str(),
                        grace_secs
                    );
                    let raw_msg = if !rule.message.trim().is_empty() {
                        &rule.message
                    } else {
                        &default_msg
                    };
                    let paragraphs =
                        self.parse_rule_message(raw_msg, &ctx, TextHorizontalAlignment::Left);
                    let mode = OverlayMode::HudTimer {
                        rule_id: "test_preview".to_string(),
                        title: format!("{}: {}", rule.action.as_str(), rule.name),
                        paragraphs,
                        duration_seconds: dur,
                        remaining_seconds: dur,
                        allow_snooze: !rule.strict_mode,
                        strict_mode: false,
                    };
                    self.show_overlay(mode);
                } else if rule.warn_before.to_seconds() > 0 {
                    let default_msg = format!(
                        "[color=#f59e0b bold] {} in {}.",
                        rule.action.as_str(),
                        &ctx.remaining
                    );
                    let raw_msg = if !rule.message.trim().is_empty() {
                        &rule.message
                    } else {
                        &default_msg
                    };
                    let paragraphs =
                        self.parse_rule_message(raw_msg, &ctx, TextHorizontalAlignment::Left);
                    let dur = rule.notification_duration.to_seconds().max(1);
                    let mode = OverlayMode::HudToast {
                        rule_id: "test_preview".to_string(),
                        title: format!("Upcoming: {}", rule.name),
                        paragraphs,
                        snooze_options_minutes: if rule.strict_mode {
                            Vec::new()
                        } else {
                            vec![5, 10, 15]
                        },
                        duration_seconds: dur,
                        remaining_seconds: dur,
                        show_time_remaining: rule.show_remaining_time,
                    };
                    self.show_overlay(mode);
                } else {
                    self.display_stats_overlay(rule.click_through).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    use crate::aw_client::AwClient;
    use crate::config::{AppConfig, DurationConfig, MonitorSelection, TimeUnit, TriggerType};
    use crate::engine::{DaemonCommand, OverlayAction};

    #[tokio::test]
    async fn test_trigger_test_overlay_modes() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let cfg = AppConfig::default();
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. "break" mode
        engine.trigger_test_overlay("break").await;
        match overlay_rx.recv().await.expect("Expected overlay action") {
            OverlayAction::Show(OverlayMode::BreakEnforcer {
                rule_id,
                duration_seconds,
                remaining_seconds,
                allow_snooze,
                strict_mode,
                ..
            }) => {
                assert_eq!(rule_id, "test_break");
                assert_eq!(duration_seconds, 300);
                assert_eq!(remaining_seconds, 300);
                assert!(allow_snooze);
                assert!(!strict_mode);
            }
            other => panic!("Expected BreakEnforcer, got: {other:?}"),
        }

        // 2. "hud_toast" mode
        engine.trigger_test_overlay("hud_toast").await;
        match overlay_rx.recv().await.expect("Expected overlay action") {
            OverlayAction::Show(OverlayMode::HudToast {
                rule_id,
                duration_seconds,
                remaining_seconds,
                show_time_remaining,
                ..
            }) => {
                assert_eq!(rule_id, "test_toast");
                assert_eq!(duration_seconds, 10);
                assert_eq!(remaining_seconds, 10);
                assert!(show_time_remaining);
            }
            other => panic!("Expected HudToast, got: {other:?}"),
        }

        // 3. "hud_timer" mode
        engine.trigger_test_overlay("hud_timer").await;
        match overlay_rx.recv().await.expect("Expected overlay action") {
            OverlayAction::Show(OverlayMode::HudTimer {
                rule_id,
                duration_seconds,
                remaining_seconds,
                allow_snooze,
                ..
            }) => {
                assert_eq!(rule_id, "test_timer");
                assert_eq!(duration_seconds, 30);
                assert_eq!(remaining_seconds, 30);
                assert!(allow_snooze);
            }
            other => panic!("Expected HudTimer, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preview_rule_alert_overlay_and_notification() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule_overlay = Rule::default_template(0);
        rule_overlay.id = "rule-overlay".to_string();
        rule_overlay.action = RuleAction::ShowOverlay;
        rule_overlay.message = "Take a break! {app_name}".to_string();

        let mut rule_notif = Rule::default_template(1);
        rule_notif.id = "rule-notif".to_string();
        rule_notif.action = RuleAction::ShowNotification;
        rule_notif.notification_method = NotificationMethod::Hud;
        rule_notif.message = "Notification for {app_name}".to_string();

        cfg.rules = vec![rule_overlay.clone(), rule_notif.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        // 1. Preview rule with ShowOverlay
        engine
            .handle_command(DaemonCommand::PreviewRule(Box::new(rule_overlay)))
            .await;

        let action1 = overlay_rx.recv().await.expect("Expected overlay action");
        match action1 {
            OverlayAction::Show(OverlayMode::BreakEnforcer {
                rule_id,
                paragraphs,
                duration_seconds,
                remaining_seconds,
                strict_mode,
                ..
            }) => {
                assert_eq!(rule_id, "test_preview");
                assert_eq!(duration_seconds, 15);
                assert_eq!(remaining_seconds, 15);
                assert!(!strict_mode);
                assert!(!paragraphs.is_empty());
            }
            other => panic!("Expected BreakEnforcer overlay, got: {other:?}"),
        }

        // 2. Preview rule with ShowNotification (Hud)
        engine
            .handle_command(DaemonCommand::PreviewRule(Box::new(rule_notif)))
            .await;

        let action2 = overlay_rx.recv().await.expect("Expected overlay action");
        match action2 {
            OverlayAction::Show(OverlayMode::HudToast {
                rule_id,
                paragraphs,
                remaining_seconds,
                ..
            }) => {
                assert_eq!(rule_id, "test_preview");
                assert_eq!(remaining_seconds, 10);
                assert!(!paragraphs.is_empty());
            }
            other => panic!("Expected HudToast overlay, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preview_rule_alert_preserves_short_warn_before_remaining() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();
        let mut rule = Rule::default_template(0);
        rule.id = "rule-short-warn".to_string();
        rule.action = RuleAction::ShowNotification;
        rule.notification_method = NotificationMethod::Hud;
        rule.trigger = TriggerType::Limit;
        rule.limit = DurationConfig::new(1, TimeUnit::Hr);
        rule.warn_before = DurationConfig::new(2, TimeUnit::Min);
        rule.message = "{remaining} remaining out of {limit}".to_string();

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        engine
            .handle_command(DaemonCommand::PreviewRule(Box::new(rule)))
            .await;

        let action = overlay_rx.recv().await.expect("Expected overlay action");
        match action {
            OverlayAction::Show(OverlayMode::HudToast { paragraphs, .. }) => {
                assert!(!paragraphs.is_empty());
                assert_eq!(paragraphs[0].text.as_str(), "2m remaining out of 1h");
            }
            other => panic!("Expected HudToast overlay, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preview_rule_alert_close_window_with_grace_and_warning() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        // 1. CloseWindow rule with grace countdown (> 0) previews HudTimer
        let mut rule_grace = Rule::default_template(0);
        rule_grace.id = "rule-cw-grace".to_string();
        rule_grace.name = "Gaming Limit".to_string();
        rule_grace.action = RuleAction::CloseWindow;
        rule_grace.grace_duration = DurationConfig::new(30, TimeUnit::Sec);
        rule_grace.message = "## Wrap up: {app}\nClosing in {remaining}.".to_string();

        cfg.rules = vec![rule_grace.clone()];
        let mut engine = Engine::new(cfg, aw_client.clone(), overlay_tx.clone());

        engine
            .handle_command(DaemonCommand::PreviewRule(Box::new(rule_grace)))
            .await;

        let action = overlay_rx.recv().await.expect("Expected overlay action");
        match action {
            OverlayAction::Show(OverlayMode::HudTimer {
                rule_id,
                title,
                paragraphs,
                duration_seconds,
                remaining_seconds,
                ..
            }) => {
                assert_eq!(rule_id, "test_preview");
                assert_eq!(title, "Close Window: Gaming Limit");
                assert_eq!(duration_seconds, 15); // clamped to 15s max
                assert_eq!(remaining_seconds, 15);
                assert!(!paragraphs.is_empty());
                assert!(paragraphs[0].text.contains("Wrap up"));
            }
            other => panic!("Expected HudTimer for CloseWindow with grace, got: {other:?}"),
        }

        // 2. CloseWindow rule with 0 grace duration previews HudToast warning
        let mut rule_warn = Rule::default_template(1);
        rule_warn.id = "rule-cw-warn".to_string();
        rule_warn.name = "Gaming Warning".to_string();
        rule_warn.action = RuleAction::CloseWindow;
        rule_warn.grace_duration = DurationConfig::new(0, TimeUnit::Sec);
        rule_warn.warn_before = DurationConfig::new(5, TimeUnit::Min);
        rule_warn.notification_duration = DurationConfig::new(10, TimeUnit::Sec);
        rule_warn.message = "Quota warning for {app}!".to_string();

        engine
            .handle_command(DaemonCommand::PreviewRule(Box::new(rule_warn)))
            .await;

        let action2 = overlay_rx.recv().await.expect("Expected overlay action");
        match action2 {
            OverlayAction::Show(OverlayMode::HudToast {
                rule_id,
                title,
                paragraphs,
                duration_seconds,
                remaining_seconds,
                ..
            }) => {
                assert_eq!(rule_id, "test_preview");
                assert_eq!(title, "Upcoming: Gaming Warning");
                assert_eq!(duration_seconds, 10);
                assert_eq!(remaining_seconds, 10);
                assert!(!paragraphs.is_empty());
                assert!(paragraphs[0].text.contains("Quota warning for"));
            }
            other => panic!("Expected HudToast for CloseWindow warning, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_preview_rule_alert_unified_context_clamping() {
        let (overlay_tx, _overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        let mut rule = Rule::default_template(0);
        rule.id = "rule-test-preview".to_string();
        rule.name = "Test Preview Rule".to_string();
        rule.action = RuleAction::CloseWindow;
        rule.warn_before = DurationConfig::new(0, TimeUnit::Sec);
        rule.grace_duration = DurationConfig::new(5, TimeUnit::Sec);
        rule.message = "Closing {app} in {remaining}!".to_string();
        rule.monitor_selection = MonitorSelection::Urls;

        cfg.rules = vec![rule.clone()];
        let mut engine = Engine::new(cfg, aw_client, overlay_tx);

        engine.preview_rule_alert(&rule).await;

        if let OverlayMode::HudTimer {
            paragraphs,
            title,
            duration_seconds,
            remaining_seconds,
            ..
        } = &engine.active_overlay_mode
        {
            assert_eq!(title, "Close Window: Test Preview Rule");
            assert_eq!(*duration_seconds, 5);
            assert_eq!(*remaining_seconds, 5);
            assert_eq!(paragraphs.len(), 1);
            assert!(paragraphs[0].text.contains("youtube.com"));
            assert!(paragraphs[0].text.contains("5s"));
        } else {
            panic!("Expected HudTimer mode for CloseWindow preview with grace_duration > 0");
        }
    }

    #[tokio::test]
    async fn test_preview_rule_alert_show_statistics() {
        let (overlay_tx, mut overlay_rx) = mpsc::unbounded_channel();
        let aw_client = Arc::new(AwClient::new(5600, ""));
        let mut cfg = AppConfig::default();

        // 1. ShowStatistics with grace_duration > 0 previews HudTimer
        let mut rule_grace = Rule::default_template(0);
        rule_grace.id = "rule-stats-grace".to_string();
        rule_grace.name = "Stats Grace Rule".to_string();
        rule_grace.action = RuleAction::ShowStatistics;
        rule_grace.grace_duration = DurationConfig::new(25, TimeUnit::Sec);

        cfg.rules = vec![rule_grace.clone()];
        let mut engine = Engine::new(cfg.clone(), aw_client.clone(), overlay_tx.clone());

        engine
            .handle_command(DaemonCommand::PreviewRule(Box::new(rule_grace)))
            .await;

        let action1 = overlay_rx.recv().await.expect("Expected overlay action");
        match action1 {
            OverlayAction::Show(OverlayMode::HudTimer {
                rule_id,
                title,
                duration_seconds,
                remaining_seconds,
                ..
            }) => {
                assert_eq!(rule_id, "test_preview");
                assert_eq!(title, "Show Statistics: Stats Grace Rule");
                assert_eq!(duration_seconds, 15); // clamped to 15s max
                assert_eq!(remaining_seconds, 15);
            }
            other => panic!("Expected HudTimer for ShowStatistics with grace, got: {other:?}"),
        }

        // 2. ShowStatistics with grace == 0 and warn_before > 0 previews HudToast
        let mut rule_warn = Rule::default_template(1);
        rule_warn.id = "rule-stats-warn".to_string();
        rule_warn.name = "Stats Warn Rule".to_string();
        rule_warn.action = RuleAction::ShowStatistics;
        rule_warn.grace_duration = DurationConfig::new(0, TimeUnit::Sec);
        rule_warn.warn_before = DurationConfig::new(5, TimeUnit::Min);
        rule_warn.notification_duration = DurationConfig::new(12, TimeUnit::Sec);

        engine
            .handle_command(DaemonCommand::PreviewRule(Box::new(rule_warn)))
            .await;

        let action2 = overlay_rx.recv().await.expect("Expected overlay action");
        match action2 {
            OverlayAction::Show(OverlayMode::HudToast {
                rule_id,
                title,
                duration_seconds,
                remaining_seconds,
                ..
            }) => {
                assert_eq!(rule_id, "test_preview");
                assert_eq!(title, "Upcoming: Stats Warn Rule");
                assert_eq!(duration_seconds, 12);
                assert_eq!(remaining_seconds, 12);
            }
            other => panic!("Expected HudToast for ShowStatistics warning, got: {other:?}"),
        }

        // 3. ShowStatistics with grace == 0 and warn_before == 0 opens HudStatus
        let mut rule_stats = Rule::default_template(2);
        rule_stats.id = "rule-stats-direct".to_string();
        rule_stats.name = "Stats Direct Rule".to_string();
        rule_stats.action = RuleAction::ShowStatistics;
        rule_stats.grace_duration = DurationConfig::new(0, TimeUnit::Sec);
        rule_stats.warn_before = DurationConfig::new(0, TimeUnit::Sec);
        rule_stats.click_through = true;

        engine
            .handle_command(DaemonCommand::PreviewRule(Box::new(rule_stats)))
            .await;

        let action3 = overlay_rx.recv().await.expect("Expected overlay action");
        match action3 {
            OverlayAction::Show(OverlayMode::HudStatus { click_through, .. }) => {
                assert!(click_through);
            }
            other => panic!("Expected HudStatus for direct ShowStatistics preview, got: {other:?}"),
        }
    }
}

