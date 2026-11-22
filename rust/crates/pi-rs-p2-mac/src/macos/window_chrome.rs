use super::title_bar;
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObjectProtocol, ProtocolObject},
};
use objc2_app_kit::{NSEvent, NSWindowButton, NSWindowStyleMask};
use objc2_foundation::{NSArray, NSNumber, NSPoint, NSRect, NSSize, NSString};
use objc2_web_kit::{
    WKContentWorld, WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKUserScript,
    WKUserScriptInjectionTime, WKWebView, WKWebViewConfiguration,
};
use std::cell::{Cell, RefCell};

const HEIGHT: f64 = 52.0;
const SOURCE: &str = r#"(() => {
 const root = document.documentElement;
 if (!root) return;
 root.dataset.shell = 'macos';
 let inset = 80, last = '', timer = 0, frame = 0, fallback = 0;
 root.style.setProperty('--shell-inset-start', '80px');
 const isBar = e => {const r=e.getBoundingClientRect(); return r.left<=1&&r.top<=1&&r.height>=24&&r.height<=80&&r.width>=120;};
 const mark = () => {
   cancelAnimationFrame(frame); clearTimeout(fallback); frame=0;
   const bars = inset>0 ? document.elementsFromPoint(2,26).filter(isBar) : [];
   const outer = bars.filter(e => !bars.some(b => b!==e&&b.contains(e)));
   for (const e of document.querySelectorAll('[data-shell-corner]')) if(!outer.includes(e)) e.removeAttribute('data-shell-corner');
   for (const e of outer) e.setAttribute('data-shell-corner','');
 };
 window.piChromeInset = n => {inset=n; root.style.setProperty('--shell-inset-start',n+'px'); mark();};
 const controls = 'a[href],button,input,textarea,select,summary,label,[role=button],[role=link],[role=tab],[role=menuitem],[role=switch],[role=slider],[contenteditable],[tabindex]:not([tabindex="-1"]),iframe,video,audio';
 const report = () => {
   timer=0; const found=new Set();
   for(const y of [10,26,42]) for(let x=2;x<innerWidth;x+=12) {const e=document.elementFromPoint(x,y)?.closest(controls); if(e)found.add(e);}
   const rects=[...found].map(e=>{const r=e.getBoundingClientRect(); return [r.left,r.top,r.width,r.height].map(Math.round);});
   const text=JSON.stringify(rects);
   if(text!==last){last=text; window.webkit.messageHandlers.piChrome.postMessage(rects);}
 };
 const schedule=()=>{if(!frame){frame=requestAnimationFrame(mark);fallback=setTimeout(mark,100);}if(!timer)timer=setTimeout(report,120);};
 addEventListener('resize',schedule); addEventListener('transitionend',schedule,true);
 addEventListener('DOMContentLoaded',()=>{new MutationObserver(schedule).observe(root,{subtree:true,childList:true,attributes:true,attributeFilter:['class','style','hidden','disabled']});schedule();});
})();"#;

#[derive(Default)]
pub(super) struct ChromeState {
    bare: Cell<bool>,
    interactive: RefCell<Vec<NSRect>>,
}

define_class!(
    // SAFETY: WKWebView is subclassable; the Rust ivars never leave the main thread.
    #[unsafe(super(WKWebView))]
    #[thread_kind = objc2::MainThreadOnly]
    #[ivars = ChromeState]
    pub(super) struct ChromeView;
    impl ChromeView {
        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self,event: &NSEvent) {
            let window = self.window();
            if self.ivars().bare.get() && let Some(window) = window
                && !window.styleMask().contains(NSWindowStyleMask::FullScreen) {
                let pt = self.convertPoint_fromView(event.locationInWindow(),None);
                // SAFETY: WKWebView is live and the mouse event was dispatched on the main thread.
                let zoom = unsafe { self.pageZoom() };
                let y = if self.isFlipped() {pt.y} else {self.bounds().size.height - pt.y};
                let p = NSPoint::new(pt.x / zoom,y/zoom);
                if y >= 0.0 && p.y < HEIGHT && !self.ivars().interactive.borrow().iter().any(|r|
                    p.x >= r.origin.x-2.0 && p.x <= r.origin.x+r.size.width+2.0 && p.y >= r.origin.y-2.0 && p.y <= r.origin.y+r.size.height+2.0) {
                    title_bar::drag(&window,event);
                    return;
                }
            }
            // SAFETY: forwards AppKit's mouseDown: to WKWebView with the same live event.
            unsafe { let _: () = msg_send![super(self), mouseDown: event]; }
        }
    }
);

impl ChromeView {
    pub(super) fn new(
        mtm: MainThreadMarker,
        configuration: &WKWebViewConfiguration,
    ) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ChromeState::default());
        // SAFETY: initialized exactly once with WKWebView's designated initializer and a live configuration.
        unsafe { msg_send![super(this), initWithFrame: NSRect::ZERO, configuration: configuration] }
    }
    pub(super) fn set_bare(&self, bare: bool) {
        self.ivars().bare.set(bare);
        self.inset();
    }
    pub(super) fn inset(&self) {
        let width = self
            .window()
            .filter(|win| {
                self.ivars().bare.get() && !win.styleMask().contains(NSWindowStyleMask::FullScreen)
            })
            .and_then(|w| w.standardWindowButton(NSWindowButton::ZoomButton))
            .and_then(|b| {
                // SAFETY: the retained button belongs to the live AppKit window.
                Some(unsafe { b.superview() }?.convertRect_toView(b.frame(), None))
            })
            .map_or(0, |r| {
                // SAFETY: this is a live WebKit view on the main thread.
                ((r.origin.x + r.size.width + 12.0) / unsafe { self.pageZoom() }) as i32
            });
        let script = NSString::from_str(&format!("window.piChromeInset?.({width})"));
        // SAFETY: JavaScript and content world are live during the call; no callback retained.
        unsafe {
            self.evaluateJavaScript_inFrame_inContentWorld_completionHandler(
                &script,
                None,
                &world(self.mtm()),
                None,
            );
        }
    }
    pub(super) fn rects(&self) -> Vec<[f64; 4]> {
        self.ivars()
            .interactive
            .borrow()
            .iter()
            .map(|r| [r.origin.x, r.origin.y, r.size.width, r.size.height])
            .collect()
    }
    pub(super) fn install(&self, controller: &WKUserContentController, mtm: MainThreadMarker) {
        let receiver = ChromeReceiver::alloc(mtm).set_ivars(objc2::rc::Weak::new(self));
        // SAFETY: initializes Object subclass and registers a handler retained by WebKit in an isolated world.
        unsafe {
            let receiver: Retained<ChromeReceiver> = msg_send![super(receiver), init];
            let script = WKUserScript::initWithSource_injectionTime_forMainFrameOnly_inContentWorld(
                WKUserScript::alloc(mtm),
                &NSString::from_str(SOURCE),
                WKUserScriptInjectionTime::AtDocumentStart,
                true,
                &world(mtm),
            );
            controller.addUserScript(&script);
            controller.addScriptMessageHandler_contentWorld_name(
                ProtocolObject::from_ref(&*receiver),
                &world(mtm),
                &NSString::from_str("piChrome"),
            );
        }
    }
}
fn world(mtm: MainThreadMarker) -> Retained<WKContentWorld> {
    // SAFETY: a fixed NSString is a valid isolated world name.
    unsafe { WKContentWorld::worldWithName(&NSString::from_str("pi-chrome"), mtm) }
}

define_class!(
    // SAFETY: NSObject subclass whose weak reference is maintained by objc2.
    #[unsafe(super(objc2_foundation::NSObject))]
    #[thread_kind = objc2::MainThreadOnly]
    #[ivars = objc2::rc::Weak<ChromeView>]
    struct ChromeReceiver;
    // SAFETY: inherits NSObject protocol selectors unchanged.
    unsafe impl NSObjectProtocol for ChromeReceiver {}
    // SAFETY: exact WebKit selector and reference lifetimes.
    unsafe impl WKScriptMessageHandler for ChromeReceiver {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn receive(&self,_controller: &WKUserContentController,message: &WKScriptMessage) {
            // SAFETY: WebKit supplies a live message, body, and frame; every class below is checked before use.
            let (main,body) = unsafe {(message.frameInfo().isMainFrame(),message.body())};
            if !main { return; }
            let Some(view) = self.ivars().load() else { return; };
            let Ok(body) = body.downcast::<NSArray<AnyObject>>() else { return; };
            let mut rects = view.ivars().interactive.borrow_mut();
            rects.clear();
            for row in body.iter().take(512) {
                let Ok(row) = row.downcast::<NSArray<AnyObject>>() else { continue; };
                if row.count()!=4 { continue; }
                let values = row.iter().map(|value|value.downcast::<NSNumber>().ok().map(|n|n.doubleValue())).collect::<Option<Vec<_>>>();
                if let Some(values) = values && let [x,y,w,h] = values.as_slice() && values.iter().all(|v|v.is_finite()) {
                    rects.push(NSRect::new(NSPoint::new(*x,*y),NSSize::new(*w,*h)));
                }
            }
        }
    }
);
