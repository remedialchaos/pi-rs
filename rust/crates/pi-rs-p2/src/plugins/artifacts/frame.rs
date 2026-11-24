//! An html artifact's page as its frame loads it: diagnostics and `window.p2view` injected,
//! served under a strict sandbox CSP.

use std::sync::LazyLock;

use regex::Regex;

use super::bridge::VIEW_BRIDGE_SCRIPT;

/// The preview's diagnostics bridge: console errors/warnings, uncaught errors, CSP violations,
/// lost WebGL contexts, and measured content height (`level: "size"`).
const DIAGNOSTIC_SCRIPT: &str = r#"(()=>{const channel="p2-artifact-diagnostic";const send=(level,args)=>{try{parent.postMessage({channel,level,message:args.map(value=>{try{return typeof value==="string"?value:JSON.stringify(value)}catch{return String(value)}}).join(" ").slice(0,4000)},"*")}catch{}};for(const level of ["error","warn"]){const original=console[level];console[level]=(...args)=>{send(level,args);original.apply(console,args)}}addEventListener("error",event=>send("error",[event.message,event.filename&&(event.filename+":"+event.lineno+":"+event.colno)].filter(Boolean)));addEventListener("unhandledrejection",event=>send("error",[event.reason?.stack||event.reason||"Unhandled promise rejection"]));addEventListener("securitypolicyviolation",event=>send("csp",[event.violatedDirective,event.blockedURI||"blocked"]));addEventListener("webglcontextlost",event=>send("webgl",["WebGL context lost"]),true);send("ready",["Preview diagnostics connected"]);const fit=()=>{const root=document.documentElement;if(root instanceof SVGSVGElement){const width=root.width.baseVal,height=root.height.baseVal,absolute=length=>length.unitType!==SVGLength.SVG_LENGTHTYPE_PERCENTAGE&&length.value>0;if(!root.hasAttribute("viewBox")&&absolute(width)&&absolute(height))root.setAttribute("viewBox","0 0 "+width.value+" "+height.value);const box=root.viewBox.baseVal;if(box&&box.width>0&&box.height>0){root.setAttribute("width","100%");root.setAttribute("height","100%")}}else if(root){const style=document.createElement("style");style.textContent=":where(body>svg[viewBox]:only-child,body>img:only-child){display:block;width:100%;height:auto}";(document.head||root).prepend(style)}};try{fit()}catch{}let reported=0;const measure=()=>{const root=document.documentElement;if(!root)return;let height;if(root instanceof SVGSVGElement){const box=root.viewBox.baseVal;height=root.height.baseVal.unitType===SVGLength.SVG_LENGTHTYPE_PERCENTAGE&&box&&box.width>0&&box.height>0?root.getBoundingClientRect().width*box.height/box.width:root.getBoundingClientRect().height}else{const overflow=root.style.overflow;root.style.overflow="hidden";height=Math.max(root.getBoundingClientRect().height,document.body?document.body.scrollHeight:0);root.style.overflow=overflow}height=Math.ceil(height);if(height===reported)return;reported=height;try{parent.postMessage({channel,level:"size",message:"",height},"*")}catch{}};const observe=()=>{try{const observer=new ResizeObserver(measure);observer.observe(document.documentElement);if(document.body)observer.observe(document.body)}catch{}measure()};addEventListener("load",measure);addEventListener("resize",measure);if(document.readyState==="loading")addEventListener("DOMContentLoaded",observe);else observe();})();"#;

/// The frame's CSP: sandboxed opaque origin with `'unsafe-eval'` for `artifact_html_eval`.
pub const FRAME_CSP: &str = "default-src 'none'; script-src 'unsafe-inline' 'unsafe-eval' 'wasm-unsafe-eval' https:; style-src 'unsafe-inline' https:; img-src data: blob: https:; font-src data: https:; media-src data: blob: https:; connect-src https:; worker-src blob:; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'; sandbox allow-scripts allow-modals allow-pointer-lock";

static HEAD_TAG: LazyLock<Regex> = LazyLock::new(|| {
    // INFALLIBLE: static regex literal verified at compile/test time.
    Regex::new("(?i)<head(?:\\s[^>]*)?>").expect("valid head tag regex")
});

/// `html` with diagnostics and the bridge at the top of `<head>` (or of the document).
pub fn frame_page(html: &str) -> String {
    let scripts =
        format!("<script>{DIAGNOSTIC_SCRIPT}</script><script>{VIEW_BRIDGE_SCRIPT}</script>");
    if let Some(found) = HEAD_TAG.find(html) {
        let mut out = String::with_capacity(html.len() + scripts.len());
        out.push_str(&html[..found.end()]);
        out.push_str(&scripts);
        out.push_str(&html[found.end()..]);
        out
    } else {
        format!("{scripts}{html}")
    }
}
