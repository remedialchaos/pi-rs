use super::{app_delegate::AppDelegate, open_url, window_chrome::ChromeView};
use crate::{
    Error, Result,
    appearance::Appearance,
    brain::{Brain, Route, url_path},
    host_bridge::{self, Message},
    local_sign_in::WebCookie,
    view_cookie,
};
use block2::{DynBlock, RcBlock};
use objc2::{
    AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, Weak},
    runtime::{AnyObject, NSObjectProtocol, ProtocolObject},
};
use objc2_app_kit::{
    NSAlert, NSButton, NSEventModifierFlags, NSModalResponseOK, NSOpenPanel, NSView,
};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSHTTPCookie, NSHTTPURLResponse, NSJSONSerialization,
    NSJSONWritingOptions, NSObject, NSRect, NSString, NSURL, NSURLRequest, NSURLResponse, NSUUID,
};
use objc2_web_kit::*;
use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    ptr,
    rc::{Rc, Weak as RcWeak},
};
use url::Url;

pub(super) struct Load {
    pub url: Url,
    pub cookie: Option<WebCookie>,
    pub gateway: Option<Url>,
}

pub(super) struct Page {
    pub id: u64,
    pub brain: Brain,
    pub web: Retained<ChromeView>,
    pub view: Retained<NSView>,
    app: Weak<AppDelegate>,
    path: RefCell<String>,
    origin: RefCell<Option<Url>>,
    gateway: RefCell<Option<Url>>,
    status: Retained<NSButton>,
    _hooks: Retained<PageHooks>,
    connecting: Cell<bool>,
    loaded: Cell<bool>,
    fallback: Cell<bool>,
    pub renewal: RefCell<Option<tokio::task::JoinHandle<()>>>,
}
impl Drop for Page {
    fn drop(&mut self) {
        if let Some(task) = self.renewal.get_mut().take() {
            task.abort();
        }
    }
}

impl Page {
    pub fn new(
        id: u64,
        brain: Brain,
        path: String,
        app: &AppDelegate,
        mtm: MainThreadMarker,
    ) -> Result<Rc<Self>> {
        let uuid = NSUUID::initWithUUIDString(
            NSUUID::alloc(),
            &NSString::from_str(&brain.datastore_id().to_string()),
        )
        .ok_or_else(|| Error::Protocol("WebKit declined brain store identifier".into()))?;
        // SAFETY: the configuration is created and used on the main thread.
        let config = unsafe { WKWebViewConfiguration::new(mtm) };
        // SAFETY: macOS 14+ provides the persistent per-UUID store; WebKit objects remain on the main thread.
        unsafe {
            config.setWebsiteDataStore(&WKWebsiteDataStore::dataStoreForIdentifier(&uuid, mtm));
            config.setApplicationNameForUserAgent(Some(&NSString::from_str("PiMac/0.0.1")));
            config.preferences().setElementFullscreenEnabled(true);
            config
                .preferences()
                .setJavaScriptCanOpenWindowsAutomatically(true);
            config.setMediaTypesRequiringUserActionForPlayback(WKAudiovisualMediaTypes::None);
        }
        let web = ChromeView::new(mtm, &config);
        let parent = NSView::initWithFrame(NSView::alloc(mtm), NSRect::ZERO);
        let hook_alloc = PageHooks::alloc(mtm).set_ivars(RefCell::new(RcWeak::new()));
        // SAFETY: initializes the NSObject subclass and assigns an action of the matching ABI to the button.
        let (hooks, status): (Retained<PageHooks>, Retained<NSButton>) = unsafe {
            let hooks: Retained<PageHooks> = msg_send![super(hook_alloc), init];
            let button = NSButton::buttonWithTitle_target_action(
                &NSString::from_str("Looking for Pi…"),
                Some(&hooks),
                Some(objc2::sel!(statusAction:)),
                mtm,
            );
            (hooks, button)
        };
        let origin = match &brain.route {
            Route::Direct { origin, .. } => Some(origin.clone()),
            _ => None,
        };
        let page = Rc::new(Self {
            id,
            brain,
            web,
            view: parent,
            app: Weak::new(app),
            path: RefCell::new(path),
            origin: RefCell::new(origin),
            gateway: RefCell::new(None),
            status,
            _hooks: hooks,
            connecting: Cell::new(false),
            loaded: Cell::new(false),
            fallback: Cell::new(false),
            renewal: RefCell::new(None),
        });
        *page._hooks.ivars().borrow_mut() = Rc::downgrade(&page);
        // SAFETY: WebKit retains the script handler, and weakly refers to retained navigation/UI delegates.
        unsafe {
            let controller = config.userContentController();
            if page.brain.is_direct() {
                page.web.install(&controller, mtm);
            }
            controller.addScriptMessageHandlerWithReply_contentWorld_name(
                ProtocolObject::from_ref(&*page._hooks),
                &WKContentWorld::pageWorld(mtm),
                &NSString::from_str(host_bridge::HANDLER),
            );
            page.web
                .setNavigationDelegate(Some(ProtocolObject::from_ref(&*page._hooks)));
            page.web
                .setUIDelegate(Some(ProtocolObject::from_ref(&*page._hooks)));
            page.web.setInspectable(true);
            page.web.setAllowsMagnification(false);
            page.web.setAllowsBackForwardNavigationGestures(true);
        }
        page.view.addSubview(&page.web);
        page.view.addSubview(&page.status);
        page.web.setHidden(true);
        Ok(page)
    }
    pub fn layout(&self, rect: NSRect, bare: bool) {
        self.view.setFrame(rect);
        let bounds = self.view.bounds();
        self.web.setFrame(bounds);
        self.status.setFrame(NSRect::new(
            objc2_foundation::NSPoint::new(20.0, (bounds.size.height - 34.0) / 2.0),
            objc2_foundation::NSSize::new((bounds.size.width - 40.0).max(0.0), 34.0),
        ));
        self.web.set_bare(bare && self.brain.is_direct());
    }
    pub fn current_path(&self) -> String {
        // SAFETY: reads the WKWebView's current URL on the main thread.
        let url = unsafe { self.web.URL() }.and_then(|u| native_url(&u));
        url.filter(|u| self.owns(u)).map_or_else(
            || self.path.borrow().clone(),
            |url| url_path(&url).to_owned(),
        )
    }
    pub fn status(&self, text: &str, action: bool) {
        self.status.setTitle(&NSString::from_str(text));
        self.status.setEnabled(action);
        self.status.setHidden(false);
        self.web.setHidden(true);
    }
    pub fn fade(&self, reduce: bool) {
        if !self.view.isHidden() {
            super::app_delegate::fade_view(&self.status, reduce);
        }
    }
    pub fn is_loaded(&self) -> bool {
        self.loaded.get()
    }
    pub fn start(&self) {
        if self.connecting.replace(true) {
            return;
        }
        self.status(
            if self.brain.is_direct() {
                "Signing in…"
            } else {
                "Opening Pi…"
            },
            false,
        );
        if let Some(app) = self.app.load() {
            app.load_page(self.id, self.brain.clone(), self.path.borrow().clone());
        }
    }
    pub fn finish_load(&self, result: Result<Load>) {
        self.connecting.set(false);
        match result {
            Ok(Load {
                url,
                cookie,
                gateway,
            }) => {
                if gateway.is_some() {
                    *self.origin.borrow_mut() = Some(url.clone());
                }
                *self.gateway.borrow_mut() = gateway;
                self.loaded.set(true);
                if let Some(cookie) = cookie {
                    self.put_cookie(cookie, Some(url));
                } else {
                    load_url(&self.web, &url);
                }
            }
            Err(error) => {
                self.fallback.set(self.brain.is_direct());
                self.status(
                    if self.brain.is_direct() {
                        "Sign In with a Token"
                    } else {
                        "Try Again"
                    },
                    true,
                );
                self.status
                    .setToolTip(Some(&NSString::from_str(&error.to_string())));
            }
        }
    }
    pub fn put_cookie(&self, cookie: WebCookie, load: Option<Url>) {
        let data = NSDictionary::from_slices(
            &[&*NSString::from_str("Set-Cookie")],
            &[&*NSString::from_str(cookie.set_cookie_header())],
        );
        let Some(url) = NSURL::URLWithString(&NSString::from_str(cookie.origin.as_str())) else {
            return;
        };
        let cookies = NSHTTPCookie::cookiesWithResponseHeaderFields_forURL(&data, &url);
        let first = cookies.firstObject();
        if let Some(first) = first {
            let web = Weak::new(&*self.web);
            let on_set = RcBlock::new(move || {
                if let (Some(url), Some(web)) = (&load, web.load()) {
                    load_url(&web, url);
                }
            });
            // SAFETY: WebKit copies the completion block; it captures only a weak view and a URL.
            unsafe {
                self.web
                    .configuration()
                    .websiteDataStore()
                    .httpCookieStore()
                    .setCookie_completionHandler(&first, Some(&on_set));
            }
        } else if let Some(url) = load {
            load_url(&self.web, &url);
        }
    }
    pub fn reload(&self) {
        if self.loaded.get() {
            // SAFETY: view is alive on the main thread.
            unsafe {
                self.web.reload();
            }
        } else {
            self.start();
        }
    }
    fn owns(&self, url: &Url) -> bool {
        self.origin
            .borrow()
            .as_ref()
            .is_some_and(|o| host_bridge::owns(o, url))
    }
    pub fn event(&self, event: &str, key: &str, value: &str) {
        // SAFETY: reads the live view's URL; do not dispatch privileged events into a foreign page.
        let url = unsafe { self.web.URL() }.and_then(|u| native_url(&u));
        if !url.as_ref().is_some_and(|url| self.owns(url)) {
            return;
        }
        let Ok(event) = serde_json::to_string(event) else {
            return;
        };
        let Ok(detail) = serde_json::to_string(&serde_json::json!({key:value})) else {
            return;
        };
        let script = NSString::from_str(&format!(
            "window.dispatchEvent(new CustomEvent({event},{{detail:{detail}}}));"
        ));
        // SAFETY: event and payload are JSON-encoded literals; call targets the main frame.
        unsafe {
            self.web.evaluateJavaScript_completionHandler(&script, None);
        }
    }
    fn action(&self) {
        if self.fallback.replace(false)
            && let Route::Direct { origin, .. } = &self.brain.route
        {
            if let Ok(url) = origin.join(&self.path.borrow()) {
                load_url(&self.web, &url);
                self.loaded.set(true);
            }
        } else if self.brain.is_direct()
            && let Some(app) = self.app.load()
            && !app.server_running()
        {
            app.start_server();
        } else {
            self.start();
        }
    }
    fn policy(&self, action: &WKNavigationAction) -> WKNavigationActionPolicy {
        // SAFETY: WebKit provides a live action/request and target frame for this callback.
        let (url, download, target, link, modified) = unsafe {
            (
                action.request().URL().and_then(|u| native_url(&u)),
                action.shouldPerformDownload(),
                action.targetFrame(),
                action.navigationType() == WKNavigationType::LinkActivated,
                action
                    .modifierFlags()
                    .contains(NSEventModifierFlags::Command)
                    || action.buttonNumber() == 2,
            )
        };
        let Some(url) = url else {
            return WKNavigationActionPolicy::Cancel;
        };
        if download {
            return WKNavigationActionPolicy::Download;
        }
        // SAFETY: the retained frame is valid; only the main frame is subject to native link handling.
        if matches!(url.scheme(), "about" | "blob" | "data")
            || target
                .as_ref()
                .is_some_and(|frame| !unsafe { frame.isMainFrame() })
        {
            return WKNavigationActionPolicy::Allow;
        }
        if let Some(path) = self
            .gateway
            .borrow()
            .as_ref()
            .and_then(|gateway| view_cookie::forward_path(gateway, &url))
        {
            *self.path.borrow_mut() = path;
            self.start();
            return WKNavigationActionPolicy::Cancel;
        }
        if !self.owns(&url) {
            let _ = open_url(&url);
            return WKNavigationActionPolicy::Cancel;
        }
        if (link && modified) || target.is_none() {
            if let Some(app) = self.app.load() {
                app.open_brain(&self.brain.id, url_path(&url));
            }
            return WKNavigationActionPolicy::Cancel;
        }
        WKNavigationActionPolicy::Allow
    }
}

fn native_url(url: &NSURL) -> Option<Url> {
    Url::parse(&url.absoluteString()?.to_string()).ok()
}
fn security_url(origin: &WKSecurityOrigin) -> Option<Url> {
    // SAFETY: WebKit supplies a valid WKSecurityOrigin during the callback.
    let (scheme, host, port) = unsafe {
        (
            origin.protocol().to_string(),
            origin.host().to_string(),
            origin.port(),
        )
    };
    if !matches!(scheme.as_str(), "http" | "https") {
        return None;
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host
    };
    let mut url = Url::parse(&format!("{scheme}://{host}/")).ok()?;
    if port != 0 {
        url.set_port(Some(u16::try_from(port).ok()?)).ok()?;
    }
    Some(url)
}
fn load_url(web: &WKWebView, url: &Url) {
    if let Some(url) = NSURL::URLWithString(&NSString::from_str(url.as_str())) {
        // SAFETY: request and view are alive on the main thread.
        unsafe {
            web.loadRequest(&NSURLRequest::requestWithURL(&url));
        }
    }
}
pub(super) fn json_value(value: &AnyObject) -> Option<serde_json::Value> {
    // SAFETY: Foundation validates the object before serialization; the result owns the bytes.
    if !unsafe { NSJSONSerialization::isValidJSONObject(value) } {
        return None;
    }
    let data = unsafe {
        NSJSONSerialization::dataWithJSONObject_options_error(value, NSJSONWritingOptions::empty())
    }
    .ok()?;
    serde_json::from_slice(&data.to_vec()).ok()
}

define_class!(
    // SAFETY: NSObject subclass with a main-thread weak Rc; it does not extend the page's lifetime.
    #[unsafe(super(NSObject))]
    #[thread_kind=objc2::MainThreadOnly]
    #[ivars=RefCell<RcWeak<Page>>]
    struct PageHooks;
    // SAFETY: NSObject selectors are inherited unchanged.
    unsafe impl NSObjectProtocol for PageHooks {}
    impl PageHooks {
        #[unsafe(method(statusAction:))]
        fn status_action(&self,_sender: &AnyObject) { if let Some(page)=self.page(){ page.action(); } }
    }
    // SAFETY: correct WorldKit selector signature and synchronous exactly-once callback delivery.
    unsafe impl WKScriptMessageHandlerWithReply for PageHooks {
        #[unsafe(method(userContentController:didReceiveScriptMessage:replyHandler:))]
        fn script(&self,_controller:&WKUserContentController,message:&WKScriptMessage,reply:&DynBlock<dyn Fn(*mut AnyObject,*mut NSString)>) {
            let response=(|| -> Result<Appearance> {
                let denied=||Error::Protocol("piHost: not allowed here".into());
                let page=self.page().ok_or_else(denied)?;
                let app=page.app.load().ok_or_else(denied)?;
                // SAFETY: read only live message content supplied by WebKit. Frame provenance comes from WebKit, not from JSON.
                let (main,origin,body)=unsafe {
                    let frame=message.frameInfo();
                    (frame.isMainFrame(),security_url(&frame.securityOrigin()),message.body())
                };
                let origin=origin.ok_or_else(denied)?;
                let action=host_bridge::admit(page.origin.borrow().as_ref(),&origin,main,&json_value(&body).unwrap_or_default())?;
                if let Message::SetTheme(appearance)=action { app.set_appearance(appearance)?; }
                Ok(app.appearance_value())
            })();
            match response {
                Ok(appearance) => {
                    let key=NSString::from_str("preference"); let value=NSString::from_str(appearance.name());
                    let body=NSDictionary::from_slices(&[&*key],&[&*value]);
                    reply.call((Retained::as_ptr(&body) as *mut AnyObject,ptr::null_mut()));
                }
                Err(error) => { let text=NSString::from_str(&error.to_string()); reply.call((ptr::null_mut(),Retained::as_ptr(&text).cast_mut())); }
            }
        }
    }
    // SAFETY: each method follows its published selector signature; policy blocks are called exactly once.
    unsafe impl WKNavigationDelegate for PageHooks {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn navigate(&self,_web:&WKWebView,action:&WKNavigationAction,reply:&DynBlock<dyn Fn(WKNavigationActionPolicy)>) {
            reply.call((self.page().map_or(WKNavigationActionPolicy::Cancel,|page|page.policy(action)),));
        }
        #[unsafe(method(webView:decidePolicyForNavigationResponse:decisionHandler:))]
        fn response(&self,_web:&WKWebView,response:&WKNavigationResponse,reply:&DynBlock<dyn Fn(WKNavigationResponsePolicy)>) {
            // SAFETY: reads the response WebKit provided and checks its dynamic type before reading HTTP headers.
            let can_show=unsafe { response.canShowMIMEType() };
            let attachment=unsafe {response.response()}.downcast::<NSHTTPURLResponse>().ok()
                .and_then(|res| res.valueForHTTPHeaderField(&NSString::from_str("Content-Disposition")))
                .is_some_and(|h|h.to_string().to_ascii_lowercase().starts_with("attachment"));
            reply.call((if can_show&&!attachment {WKNavigationResponsePolicy::Allow}else{WKNavigationResponsePolicy::Download},));
        }
        #[unsafe(method(webView:didCommitNavigation:))]
        fn committed(&self,_web:&WKWebView,_navigation:Option<&WKNavigation>) {
            if let Some(page)=self.page() { page.status.setHidden(true);page.web.setHidden(false);page.web.inset(); }
        }
        #[unsafe(method(webView:didFinishNavigation:))]
        fn finished(&self,_web:&WKWebView,_navigation:Option<&WKNavigation>) {
            if let Some(page)=self.page() && let Some(app)=page.app.load() { app.diagnose(&page); }
        }
        #[unsafe(method(webView:didFailProvisionalNavigation:withError:))]
        fn failed(&self,_web:&WKWebView,_navigation:Option<&WKNavigation>,error:&NSError) {
            if error.code()!=-999 && let Some(page)=self.page(){page.loaded.set(false);page.status("Cannot reach Pi · Try Again",true);page.status.setToolTip(Some(&error.localizedDescription()));}
        }
        #[unsafe(method(webViewWebContentProcessDidTerminate:))]
        fn crashed(&self,web:&WKWebView) {
            // SAFETY: view is valid during this callback; WebKit can reload after its renderer restarts.
            unsafe {web.reload();}
        }
        #[unsafe(method(webView:navigationAction:didBecomeDownload:))]
        fn action_download(&self,_web:&WKWebView,_action:&WKNavigationAction,download:&WKDownload) {
            // SAFETY: this object conforms to WKDownloadDelegate and is retained by its page.
            unsafe { download.setDelegate(Some(ProtocolObject::from_ref(self))); }
        }
        #[unsafe(method(webView:navigationResponse:didBecomeDownload:))]
        fn response_download(&self,_web:&WKWebView,_response:&WKNavigationResponse,download:&WKDownload) {
            // SAFETY: this object conforms to WKDownloadDelegate and is retained by its page.
            unsafe { download.setDelegate(Some(ProtocolObject::from_ref(self))); }
        }
    }
    // SAFETY: the UI delegates use the declared signatures; every decision block is called once.
    unsafe impl WKUIDelegate for PageHooks {
        #[unsafe(method_id(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        fn new_web(&self,_web:&WKWebView,_config:&WKWebViewConfiguration,action:&WKNavigationAction,_features:&WKWindowFeatures) -> Option<Retained<WKWebView>> {
            // SAFETY: WebKit supplies the request until this callback returns.
            if let Some(url)=unsafe {action.request().URL()}.and_then(|u|native_url(&u)) && url.scheme()!="about" && let Some(page)=self.page() {
                if page.owns(&url) {if let Some(app)=page.app.load(){app.open_brain(&page.brain.id,url_path(&url));}}
                else {let _=open_url(&url);}
            }
            None
        }
        #[unsafe(method(webView:requestMediaCapturePermissionForOrigin:initiatedByFrame:type:decisionHandler:))]
        fn media(&self,_web:&WKWebView,origin:&WKSecurityOrigin,_frame:&WKFrameInfo,_kind:WKMediaCaptureType,reply:&DynBlock<dyn Fn(WKPermissionDecision)>) {
            let grant=self.page().is_some_and(|page|security_url(origin).is_some_and(|origin|page.owns(&origin)));
            reply.call((if grant {WKPermissionDecision::Grant}else{WKPermissionDecision::Deny},));
        }
        #[unsafe(method(webView:runOpenPanelWithParameters:initiatedByFrame:completionHandler:))]
        fn files(&self,_web:&WKWebView,parameters:&WKOpenPanelParameters,_frame:&WKFrameInfo,reply:&DynBlock<dyn Fn(*mut NSArray<NSURL>)>) {
            let panel=NSOpenPanel::openPanel(self.mtm());
            // SAFETY: WebKit owns parameters for the duration of this callback.
            unsafe { panel.setAllowsMultipleSelection(parameters.allowsMultipleSelection()); panel.setCanChooseDirectories(parameters.allowsDirectories()); }
            let selected=(panel.runModal()==NSModalResponseOK).then(||panel.URLs());
            reply.call((selected.as_ref().map_or(ptr::null_mut(),|a|Retained::as_ptr(a).cast_mut()),));
        }
        #[unsafe(method(webView:runJavaScriptAlertPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn alert(&self,_web:&WKWebView,message:&NSString,_frame:&WKFrameInfo,reply:&DynBlock<dyn Fn()>) {
            self.dialog(message,false,None); reply.call(());
        }
        #[unsafe(method(webView:runJavaScriptConfirmPanelWithMessage:initiatedByFrame:completionHandler:))]
        fn confirm(&self,_web:&WKWebView,message:&NSString,_frame:&WKFrameInfo,reply:&DynBlock<dyn Fn(objc2::runtime::Bool)>) {
            reply.call((objc2::runtime::Bool::new(self.dialog(message,true,None)),));
        }
        #[unsafe(method(webView:runJavaScriptTextInputPanelWithPrompt:defaultText:initiatedByFrame:completionHandler:))]
        fn prompt(&self,_web:&WKWebView,message:&NSString,default:Option<&NSString>,_frame:&WKFrameInfo,reply:&DynBlock<dyn Fn(*mut NSString)>) {
            let field=objc2_app_kit::NSTextField::textFieldWithString(default.unwrap_or(&NSString::from_str("")),self.mtm());
            field.setFrameSize(objc2_foundation::NSSize::new(280.0,24.0));
            let value=self.dialog(message,true,Some(&field)).then(||field.stringValue());
            reply.call((value.as_ref().map_or(ptr::null_mut(),|s|Retained::as_ptr(s).cast_mut()),));
        }
    }
    // SAFETY: valid download callback ABI; returned URL stays retained until the completion call returns.
    unsafe impl WKDownloadDelegate for PageHooks {
        #[unsafe(method(download:decideDestinationUsingResponse:suggestedFilename:completionHandler:))]
        fn destination(&self,_download:&WKDownload,_response:&NSURLResponse,name:&NSString,reply:&DynBlock<dyn Fn(*mut NSURL)>) {
            let name=name.to_string();
            let file=Path::new(&name).file_name().and_then(|p|p.to_str()).filter(|s|!s.is_empty());
            let url=file.and_then(|file|{
                let home=std::env::var_os("HOME").map(PathBuf::from)?;
                let downloads=home.join("Downloads");
                let path=Path::new(file);
                let mut candidate=downloads.join(path);
                let stem=path.file_stem()?.to_str()?;
                for i in 2..=10000 {
                    if !candidate.exists(){return Some(NSURL::fileURLWithPath(&NSString::from_str(&candidate.to_string_lossy())));}
                    let name=path.extension().and_then(|s|s.to_str()).map_or_else(||format!("{stem} {i}"),|ext|format!("{stem} {i}.{ext}"));
                    candidate=downloads.join(name);
                }
                None
            });
            reply.call((url.as_ref().map_or(ptr::null_mut(),|u|Retained::as_ptr(u).cast_mut()),));
        }
    }
);
impl PageHooks {
    fn page(&self) -> Option<Rc<Page>> {
        self.ivars().borrow().upgrade()
    }
    fn dialog(&self, message: &NSString, cancel: bool, accessory: Option<&NSView>) -> bool {
        let alert = NSAlert::new(self.mtm());
        alert.setMessageText(message);
        alert.addButtonWithTitle(&NSString::from_str("OK"));
        if cancel {
            alert.addButtonWithTitle(&NSString::from_str("Cancel"));
        }
        alert.setAccessoryView(accessory);
        alert.runModal() == objc2_app_kit::NSAlertFirstButtonReturn
    }
}
