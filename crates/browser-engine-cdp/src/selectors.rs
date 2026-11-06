//! The retained driver's pinned, browser-only selector evaluator runs in an isolated world.
//! Rust owns target discovery, frame traversal, deadlines and remote object lifetimes.
use super::target::{Control, FrameTarget, Renderer};
use super::*;
#[derive(Clone)]
pub(super) struct NodeRef {
    pub page: FrameTarget,
    pub ancestors: Vec<(FrameTarget, i64)>,
    pub object_id: String,
}
async fn world(page: &impl Control, frame: &str) -> Result<i64> {
    let result = command(
        page,
        "Page.createIsolatedWorld",
        json!({"frameId":frame,"worldName":"browser-locator","grantUniveralAccess":false}),
    )
    .await?;
    result
        .get("executionContextId")
        .and_then(Value::as_i64)
        .ok_or_else(|| anyhow!("Frame context unavailable"))
}
fn runtime() -> String {
    format!("globalThis.__browserLocator || (globalThis.__browserLocator=(()=>{{const module={{exports:{{}}}};{};return new (module.exports.InjectedScript())(globalThis,{{isUnderTest:false,sdkLanguage:'javascript',testIdAttributeName:'data-testid',stableRafCount:1,browserName:'chromium',customEngines:[],isUtilityWorld:true}})}})())",include_str!("vendor/dom-runtime.js"))
}
async fn evaluate_context(
    page: &impl Control,
    context: i64,
    expression: String,
    by_value: bool,
) -> Result<Value> {
    let result=command(page,"Runtime.evaluate",json!({"expression":expression,"contextId":context,"returnByValue":by_value,"awaitPromise":true})).await?;
    if result.get("exceptionDetails").is_some() {
        bail!("That element is not on the page.")
    }
    Ok(result)
}
pub(super) async fn resolve(
    browser: &Browser,
    page: &Page,
    selector: Option<&str>,
) -> Result<NodeRef> {
    let tree = command(page, "Page.getFrameTree", json!({})).await?;
    let mut frame = tree
        .pointer("/frameTree/frame/id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Frame unavailable"))?
        .to_owned();
    let mut page = FrameTarget::Main(page.clone());
    let mut ancestors = Vec::new();
    let mut context = world(&page, &frame).await?;
    let selector = selector.map(|s| format!("{s} >> nth=0"));
    let segments = if let Some(selector) = &selector {
        let script=format!("(()=>{{const injected={};const parsed=injected.parseSelector({});const groups=[{{parts:[]}}];for(let i=0;i<parsed.parts.length;i++){{const part=parsed.parts[i];if(part.name==='internal:control'&&part.body==='enter-frame')groups.push({{parts:[]}});else{{const group=groups[groups.length-1];if(parsed.capture===i)group.capture=group.parts.length;group.parts.push(part)}}}}return groups}})()",runtime(),json!(selector));
        evaluate_context(&page, context, script, true)
            .await?
            .pointer("/result/value")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| anyhow!("Selector unavailable"))?
    } else {
        Vec::new()
    };
    for (index, parts) in segments.iter().enumerate() {
        let last = index + 1 == segments.len();
        let script = format!(
            "(()=>{{const injected={};return injected.querySelector({},document,{})}})()",
            runtime(),
            parts,
            !last
        );
        let result = evaluate_context(&page, context, script, false).await?;
        let object_id = result
            .pointer("/result/objectId")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("That element is not on the page."))?
            .to_owned();
        if last {
            return Ok(NodeRef {
                page,
                object_id,
                ancestors,
            });
        }
        let owner = command(&page, "DOM.describeNode", json!({"objectId":object_id})).await;
        let _ = command(
            &page,
            "Runtime.releaseObject",
            json!({"objectId":object_id}),
        )
        .await;
        let owner = owner?;
        frame = owner
            .pointer("/node/frameId")
            .or_else(|| owner.pointer("/node/contentDocument/frameId"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("Not a frame owner"))?
            .to_owned();
        context = match world(&page, &frame).await {
            Ok(context) => context,
            Err(_) => {
                let backend = owner
                    .pointer("/node/backendNodeId")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| anyhow!("Frame owner unavailable"))?;
                ancestors.push((page.clone(), backend));
                page = FrameTarget::Child(Arc::new(
                    Renderer::connect(browser.websocket_address(), &frame).await?,
                ));
                world(&page, &frame).await?
            }
        };
    }
    let result = evaluate_context(&page, context, "document.activeElement".into(), false).await?;
    let object_id = result
        .pointer("/result/objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("Focus unavailable"))?
        .to_owned();
    Ok(NodeRef {
        page,
        object_id,
        ancestors,
    })
}
