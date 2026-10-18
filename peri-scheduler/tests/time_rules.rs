use peri_scheduler::Timing;
fn utc(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s).unwrap().timestamp()
}
#[test]
fn explicit_offsets_intervals_and_dst_remain_stable() {
    assert!(Timing::Once {
        at: "2026-10-18T14:00:00".into()
    }
    .next_after(0)
    .is_err());
    let interval = Timing::Interval {
        anchor: 100,
        every_seconds: 60,
    };
    assert_eq!(interval.next_after(100).unwrap(), Some(160));
    assert_eq!(interval.next_after(245).unwrap(), Some(280));
    let cron = Timing::Cron {
        expression: "0 9 * * *".into(),
        timezone: "America/New_York".into(),
    };
    assert_eq!(
        cron.next_after(utc("2026-03-07T14:00:00Z")).unwrap(),
        Some(utc("2026-03-08T13:00:00Z"))
    );
    assert_eq!(
        cron.next_after(utc("2026-10-31T13:00:00Z")).unwrap(),
        Some(utc("2026-11-01T14:00:00Z"))
    );
    for (expression, timezone) in [
        ("* * * * * *", "UTC"),
        ("61 * * * *", "UTC"),
        ("* * * * *", "server-local"),
    ] {
        assert!(Timing::Cron {
            expression: expression.into(),
            timezone: timezone.into()
        }
        .next_after(0)
        .is_err());
    }
}

#[test]
fn missing_wall_time_is_skipped_and_repeated_wall_time_fires_only_at_earlier_instant() {
    let gap = Timing::Cron {
        expression: "30 2 * * *".into(),
        timezone: "America/New_York".into(),
    };
    assert_eq!(
        gap.next_after(utc("2026-03-08T06:00:00Z")).unwrap(),
        Some(utc("2026-03-09T06:30:00Z"))
    );
    let repeat = Timing::Cron {
        expression: "30 1 * * *".into(),
        timezone: "America/New_York".into(),
    };
    assert_eq!(
        repeat.next_after(utc("2026-11-01T05:00:00Z")).unwrap(),
        Some(utc("2026-11-01T05:30:00Z"))
    );
    assert_eq!(
        repeat.next_after(utc("2026-11-01T06:10:00Z")).unwrap(),
        Some(utc("2026-11-02T06:30:00Z"))
    );
    let wildcard = Timing::Cron {
        expression: "* * * * *".into(),
        timezone: "America/New_York".into(),
    };
    assert_eq!(
        wildcard.next_after(utc("2026-11-01T06:10:00Z")).unwrap(),
        Some(utc("2026-11-01T07:00:00Z"))
    );
}
