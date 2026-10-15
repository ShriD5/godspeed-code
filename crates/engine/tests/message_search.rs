//! `SearchMessages` against a real engine: a message written through the
//! doc host is findable within the live debounce, only registry chats are
//! indexed, and the hosted-only / chat-id scopes filter as the viewport's
//! fan-out expects.

use std::sync::Arc;
use std::time::{Duration, Instant};

use zeron_engine::{EngineCore, HarnessRegistry};
use zeron_proto::{HarnessId, MessageSearchRole, SearchMessagesResult};
use zeron_rpc::methods;

async fn search(
    client: &zeron_rpc::RpcClient,
    params: serde_json::Value,
) -> SearchMessagesResult {
    let value = client
        .call(methods::SEARCH_MESSAGES, params)
        .await
        .expect("SearchMessages");
    serde_json::from_value(value).expect("SearchMessagesResult")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_messages_are_searchable_through_the_rpc() {
    let tmp = tempfile::tempdir().unwrap();
    let core = EngineCore::assemble(
        &tmp.path().join("data"),
        Arc::new(HarnessRegistry::new()),
        HarnessId::Mock,
        None,
    )
    .expect("engine core assembles");
    let client = zeron_rpc::memory_client(core.rpc_service());
    client
        .call(
            methods::MUTATE,
            serde_json::json!({
                "op": "createChat",
                "chatId": "chat-search",
                "deviceId": core.device_id,
            }),
        )
        .await
        .expect("createChat");
    core.doc_host
        .open_local("chat-search")
        .unwrap()
        .write_user_message("m1", "Where do the Lanterns keep their oil?", 1)
        .unwrap();
    // A document without a registry row (a rollback copy, say) stays out.
    core.doc_host
        .open_local("not-a-chat")
        .unwrap()
        .write_user_message("m2", "lanterns everywhere", 2)
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let result = loop {
        let result = search(&client, serde_json::json!({ "query": "l" })).await;
        if !result.indexing && !result.hits.is_empty() {
            break result;
        }
        assert!(Instant::now() < deadline, "message never indexed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(result.hits.len(), 1, "{:?}", result.hits);
    let hit = &result.hits[0];
    assert_eq!((hit.chat_id.as_str(), hit.message_id.as_str()), ("chat-search", "m1"));
    assert_eq!(hit.role, MessageSearchRole::User);
    let (start, end) = hit.ranges[0];
    assert_eq!(&hit.snippet[start as usize..end as usize], "L");

    let hosted = search(
        &client,
        serde_json::json!({ "query": "lantern oil", "hostedOnly": true }),
    )
    .await;
    assert_eq!(hosted.hits.len(), 1);
    let elsewhere = search(
        &client,
        serde_json::json!({ "query": "lantern", "chatIds": ["other"] }),
    )
    .await;
    assert!(elsewhere.hits.is_empty());
    let too_long = client
        .call(
            methods::SEARCH_MESSAGES,
            serde_json::json!({ "query": "x".repeat(300) }),
        )
        .await;
    assert!(too_long.is_err());
}
