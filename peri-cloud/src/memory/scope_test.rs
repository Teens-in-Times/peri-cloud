use super::tests::{fixture, value};
use super::*;
use peri_acp_types::device_enrollment::DeviceRecord;

pub(super) async fn device(journal: &crate::state::CloudJournal, owner: Uuid) -> DeviceRecord {
    let device = DeviceRecord {
        id: Uuid::new_v4(),
        name: "小蓝".into(),
        platform: "linux".into(),
        default_workspace: "/private/path/not/model/catalog".into(),
        connection_id: Some(Uuid::new_v4()),
        is_local: false,
        revoked: false,
    };
    sqlx::query("INSERT INTO identity_devices(id,principal,record_json) VALUES(?,?,?)")
        .bind(device.id.to_string())
        .bind(owner.to_string())
        .bind(serde_json::to_string(&device).unwrap())
        .execute(&journal.identity_pool())
        .await
        .unwrap();
    device
}

#[tokio::test]
async fn scopes_authorize_every_operation_and_share_account_capacity() {
    let (_root, journal, a, b) = fixture().await;
    let d = device(&journal, a).await;
    let user = MemoryStore::new(&journal, a, 65536);
    let fleet = user.clone().scoped(MemoryScope::Fleet);
    let env = user
        .clone()
        .scoped(MemoryScope::DeviceEnv { device_id: d.id });
    let mut records = Vec::new();
    for (generation, store) in [&user, &fleet, &env].into_iter().enumerate() {
        records.push(
            store
                .add(
                    Uuid::new_v4(),
                    generation as u64,
                    value("小蓝 Python /srv/test"),
                    MemorySource::web(),
                )
                .await
                .unwrap(),
        );
    }
    for (store, record) in [&user, &fleet, &env].into_iter().zip(&records) {
        let page = store.list("小蓝", 0, 10).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].id, record.id);
        assert_eq!(page.generation, 3);
        assert_eq!(page.total, 3);
    }
    assert!(matches!(
        user.get(records[2].id).await,
        Err(MemoryError::NotFound)
    ));
    assert!(matches!(
        user.update(
            records[2].id,
            1,
            value("scope cannot move"),
            MemorySource::web()
        )
        .await,
        Err(MemoryError::Conflict)
    ));
    let foreign =
        MemoryStore::new(&journal, b, 65536).scoped(MemoryScope::DeviceEnv { device_id: d.id });
    assert!(matches!(
        foreign.list("", 0, 1).await,
        Err(MemoryError::Forbidden)
    ));
    assert!(matches!(
        foreign
            .add(
                Uuid::new_v4(),
                0,
                value("unauthorized"),
                MemorySource::web()
            )
            .await,
        Err(MemoryError::Forbidden)
    ));
    assert!(matches!(
        user.clone()
            .scoped(MemoryScope::Context {
                context_id: Uuid::new_v4()
            })
            .snapshot()
            .await,
        Err(MemoryError::Forbidden)
    ));
    let quota = MemoryStore::new(&journal, a, 10).scoped(MemoryScope::Fleet);
    assert!(matches!(
        quota
            .add(
                Uuid::new_v4(),
                3,
                value("capacity is global"),
                MemorySource::web()
            )
            .await,
        Err(MemoryError::Limit)
    ));
    let mut revoked = d;
    revoked.revoked = true;
    sqlx::query("UPDATE identity_devices SET record_json=? WHERE id=?")
        .bind(serde_json::to_string(&revoked).unwrap())
        .bind(revoked.id.to_string())
        .execute(&journal.identity_pool())
        .await
        .unwrap();
    assert!(matches!(
        env.get(records[2].id).await,
        Err(MemoryError::Forbidden)
    ));
    assert!(matches!(
        env.update(records[2].id, 1, value("revoked"), MemorySource::web())
            .await,
        Err(MemoryError::Forbidden)
    ));
    assert!(matches!(
        env.forget(records[2].id, 1, MemorySource::web()).await,
        Err(MemoryError::Forbidden)
    ));
    assert!(matches!(env.snapshot().await, Err(MemoryError::Forbidden)));
    let management = env.clone().for_management();
    assert_eq!(management.inventory("", 0, 5).await.unwrap().items.len(), 1);
    assert!(matches!(
        management.get(records[2].id).await,
        Err(MemoryError::Forbidden)
    ));
    assert!(matches!(
        foreign.for_management().inspect(records[2].id).await,
        Err(MemoryError::Forbidden)
    ));
    management
        .forget(records[2].id, 1, MemorySource::web())
        .await
        .unwrap();
    assert!(management
        .inventory("", 0, 5)
        .await
        .unwrap()
        .items
        .is_empty());
}
