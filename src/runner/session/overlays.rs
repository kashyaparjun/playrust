#![allow(unused_imports)]
use std::time::{Duration, Instant};

use super::super::context::ActiveContext;
use super::super::state::RuntimeState;
use super::super::{SECONDARY_TIMEOUT, StepError, protocol};
use crate::flow::{Assertion, CompiledStep, Operation, PresentationOverlays, Redactor};
use crate::report::FailureCategory;

pub(crate) async fn pause_until(duration: Duration, deadline: Instant) -> Result<(), StepError> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if duration > remaining {
        tokio::time::sleep(remaining).await;
        return Err(StepError::new(FailureCategory::Timeout, "step deadline expired").deadline());
    }
    tokio::time::sleep(duration).await;
    Ok(())
}

pub(crate) async fn deactivate_presentation_overlay(
    active: &ActiveContext,
    runtime: &mut RuntimeState,
) {
    runtime.presentation_overlay_recording = false;
    let _ = remove_presentation_overlay(active).await;
}

pub(crate) fn step_captures_screenshot(step: &CompiledStep) -> bool {
    matches!(
        step.operation,
        Operation::Screenshot { .. } | Operation::Assert(Assertion::Screenshot(_))
    )
}

pub(crate) async fn update_presentation_overlay(
    active: &ActiveContext,
    step: &CompiledStep,
    overlays: &PresentationOverlays,
    redactor: &Redactor,
) -> Result<(), StepError> {
    let url = if overlays.url {
        redactor.redact(&active.url().await.map_err(protocol)?.unwrap_or_default())
    } else {
        String::new()
    };
    let step_text = if overlays.step {
        redactor.redact(&format!(
            "Step {}{}",
            step.index,
            step.id
                .as_deref()
                .map_or(String::new(), |id| format!(" · {id}"))
        ))
    } else {
        String::new()
    };
    // ponytail: values are JSON-serialized before injection; any new dynamic
    // value must go through serde_json::to_string to stay injection-safe.
    let script = format!(
        r#"(() => {{
            const tag = 'playrust-presentation-overlay';
            let host = document.querySelector(`${{tag}}[data-playrust-presentation-overlay]`);
            if (!host) {{
                if (typeof document.__playrustPresentationOverlayCleanup === 'function') {{
                    document.__playrustPresentationOverlayCleanup();
                }}
                host = document.createElement(tag);
                host.dataset.playrustPresentationOverlay = '';
                host.setAttribute('aria-hidden', 'true');
                host.style.cssText = 'all:initial;display:contents;pointer-events:none';
                const shadow = host.attachShadow({{ mode: 'open' }});
                const style = document.createElement('style');
                style.textContent = `
                    :host, * {{ box-sizing:border-box;pointer-events:none }}
                    #context {{ position:fixed;left:0;right:0;top:0;display:flex;gap:16px;padding:10px 14px;background:rgba(0,0,0,.72);font:600 14px sans-serif;color:white;text-shadow:0 1px 2px #000;white-space:nowrap;overflow:hidden }}
                    #pointer {{ position:fixed;width:18px;height:18px;border:3px solid #ff3b30;border-radius:50%;transform:translate(-50%,-50%);left:50%;top:50% }}
                    [data-marker="click"] {{ position:fixed;width:34px;height:34px;border:4px solid #34c759;border-radius:50%;transform:translate(-50%,-50%);box-shadow:0 0 0 5px rgba(52,199,89,.28) }}
                    [data-marker="scroll"] {{ position:fixed;min-width:44px;padding:8px 12px;border-radius:22px;transform:translate(-50%,-50%);background:#ffd60a;font:700 22px sans-serif;color:#111;text-align:center }}
                `;
                const context = document.createElement('div');
                context.id = 'context';
                const pointer = document.createElement('div');
                pointer.id = 'pointer';
                const markers = document.createElement('div');
                markers.id = 'markers';
                shadow.append(style, context, pointer, markers);

                const showMarker = (kind, x, y, text) => {{
                    markers.querySelector(`[data-marker="${{kind}}"]`)?.remove();
                    const marker = document.createElement('div');
                    marker.dataset.marker = kind;
                    if (Number.isFinite(x)) marker.style.left = `${{x}}px`;
                    if (Number.isFinite(y)) marker.style.top = `${{y}}px`;
                    marker.textContent = text;
                    markers.appendChild(marker);
                }};
                const onMove = event => {{
                    pointer.style.left = `${{event.clientX}}px`;
                    pointer.style.top = `${{event.clientY}}px`;
                }};
                const onPointerDown = event => showMarker('click', event.clientX, event.clientY, '');
                const onWheel = event => showMarker(
                    'scroll',
                    innerWidth / 2,
                    innerHeight / 2,
                    Math.abs(event.deltaY) >= Math.abs(event.deltaX)
                        ? (event.deltaY >= 0 ? '↓' : '↑')
                        : (event.deltaX >= 0 ? '→' : '←')
                );
                if ({pointer}) {{
                    document.addEventListener('pointermove', onMove, true);
                    document.addEventListener('pointerdown', onPointerDown, true);
                    document.addEventListener('wheel', onWheel, true);
                    document.__playrustPresentationOverlayCleanup = () => {{
                        document.removeEventListener('pointermove', onMove, true);
                        document.removeEventListener('pointerdown', onPointerDown, true);
                        document.removeEventListener('wheel', onWheel, true);
                    }};
                }}
                document.documentElement.appendChild(host);
            }}
            const shadow = host.shadowRoot;
            const context = shadow.getElementById('context');
            context.replaceChildren();
            const add = value => {{
                if (!value) return;
                const item = document.createElement('span');
                item.textContent = value;
                context.appendChild(item);
            }};
            add({step});
            add({url});
            shadow.getElementById('pointer').hidden = !{pointer};
        }})()"#,
        step = serde_json::to_string(&step_text).expect("overlay step serializes"),
        url = serde_json::to_string(&url).expect("overlay URL serializes"),
        pointer = overlays.pointer,
    );
    tokio::time::timeout(SECONDARY_TIMEOUT, active.page.evaluate(script))
        .await
        .map_err(|_| protocol("presentation overlay update timed out"))?
        .map_err(protocol)?;
    Ok(())
}

pub(crate) async fn remove_presentation_overlay(active: &ActiveContext) -> Result<(), StepError> {
    tokio::time::timeout(
        SECONDARY_TIMEOUT,
        active.page.evaluate(
            r#"(() => {
                if (typeof document.__playrustPresentationOverlayCleanup === 'function') {
                    document.__playrustPresentationOverlayCleanup();
                    delete document.__playrustPresentationOverlayCleanup;
                }
                document.querySelector('playrust-presentation-overlay[data-playrust-presentation-overlay]')?.remove();
            })()"#,
        ),
    )
    .await
    .map_err(|_| protocol("presentation overlay removal timed out"))?
    .map_err(protocol)?;
    Ok(())
}
