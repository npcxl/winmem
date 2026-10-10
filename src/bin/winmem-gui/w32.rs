//! Win32 / GDI / Common Controls 原生绑定（GUI 专用，零第三方依赖）。
#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::c_void;

// ---------------------------------------------------------------------------
// 基础类型
// ---------------------------------------------------------------------------

pub type HWND = isize;
pub type HINSTANCE = isize;
pub type HMENU = isize;
pub type HCURSOR = isize;
pub type HBRUSH = isize;
pub type HFONT = isize;
pub type HGDIOBJ = isize;
pub type HDC = isize;
pub type ATOM = u16;
pub type LPARAM = isize;
pub type WPARAM = usize;
pub type LRESULT = isize;
pub type BOOL = i32;
pub type UINT = u32;

// ---------------------------------------------------------------------------
// 常量
// ---------------------------------------------------------------------------

// 窗口样式
pub const WS_OVERLAPPEDWINDOW: u32 = 0x00CF_0000;
pub const WS_CHILD: u32 = 0x4000_0000;
pub const WS_VISIBLE: u32 = 0x1000_0000;
pub const WS_TABSTOP: u32 = 0x0001_0000;
pub const WS_BORDER: u32 = 0x0080_0000;
pub const WS_CLIPCHILDREN: u32 = 0x0200_0000;
pub const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
pub const ES_AUTOHSCROLL: u32 = 0x0080;
pub const BS_PUSHBUTTON: u32 = 0x0000_0000;
pub const CS_HREDRAW: u32 = 0x0002;
pub const CS_VREDRAW: u32 = 0x0001;
pub const SW_SHOW: i32 = 5;
pub const CW_USEDEFAULT: i32 = i32::MIN;

// 消息
pub const WM_CREATE: u32 = 0x0001;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_PAINT: u32 = 0x000F;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_ERASEBKGND: u32 = 0x0014;
pub const WM_GETMINMAXINFO: u32 = 0x0024;
pub const WM_SETFONT: u32 = 0x0030;
pub const WM_NOTIFY: u32 = 0x004E;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_TIMER: u32 = 0x0113;
pub const EN_CHANGE: u32 = 0x0300;
pub const BN_CLICKED: u32 = 0;
pub const EM_SETCUEBANNER: u32 = 0x1501;

// ListView 通知
pub const LVN_ITEMCHANGED: i32 = -101;
pub const LVN_COLUMNCLICK: i32 = -108;

// ListView 样式与消息
pub const LVS_REPORT: u32 = 0x0001;
pub const LVS_SHOWSELALWAYS: u32 = 0x0008;
pub const LVS_EX_GRIDLINES: u32 = 0x0000_0001;
pub const LVS_EX_FULLROWSELECT: u32 = 0x0000_0020;
pub const LVS_EX_LABELTIP: u32 = 0x0000_4000;
pub const LVS_EX_DOUBLEBUFFER: u32 = 0x0001_0000;

pub const LVM_FIRST: u32 = 0x1000;
pub const LVM_GETITEMCOUNT: u32 = LVM_FIRST + 4;
pub const LVM_DELETEALLITEMS: u32 = LVM_FIRST + 9;
pub const LVM_GETNEXTITEM: u32 = LVM_FIRST + 12;
pub const LVM_SETITEMSTATE: u32 = LVM_FIRST + 43;
pub const LVM_SETEXTENDEDLISTVIEWSTYLE: u32 = LVM_FIRST + 54;
pub const LVM_INSERTITEMW: u32 = LVM_FIRST + 77;
pub const LVM_INSERTCOLUMNW: u32 = LVM_FIRST + 97;
pub const LVM_SETITEMTEXTW: u32 = LVM_FIRST + 116;

pub const LVIF_TEXT: u32 = 0x0001;
pub const LVCF_FMT: u32 = 0x0001;
pub const LVCF_WIDTH: u32 = 0x0002;
pub const LVCF_TEXT: u32 = 0x0004;
pub const LVCF_SUBITEM: u32 = 0x0008;
pub const LVCFMT_LEFT: i32 = 0;
pub const LVCFMT_RIGHT: i32 = 2;
pub const LVNI_SELECTED: i32 = 0x0002;
pub const LVIS_SELECTED: u32 = 0x0002;
pub const LVIS_FOCUSED: u32 = 0x0001;

// MessageBox
pub const MB_OK: u32 = 0x0000;
pub const MB_YESNO: u32 = 0x0004;
pub const MB_ICONWARNING: u32 = 0x0030;
pub const MB_ICONINFORMATION: u32 = 0x0040;
pub const IDYES: i32 = 6;

// GDI
pub const TRANSPARENT: i32 = 1;
pub const DEFAULT_CHARSET: u32 = 1;
pub const FW_NORMAL: i32 = 400;
pub const FW_SEMIBOLD: i32 = 600;
pub const DT_LEFT: u32 = 0x0000;
pub const DT_CENTER: u32 = 0x0001;
pub const DT_RIGHT: u32 = 0x0002;
pub const DT_VCENTER: u32 = 0x0004;
pub const DT_SINGLELINE: u32 = 0x0020;
pub const DT_END_ELLIPSIS: u32 = 0x8000;

pub const ICC_LISTVIEW_CLASSES: u32 = 0x0000_0001;
pub const GWLP_USERDATA: i32 = -21;
pub const IDC_ARROW: usize = 32512;

/// COLORREF：0x00BBGGRR
pub const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

// ---------------------------------------------------------------------------
// 结构体
// ---------------------------------------------------------------------------

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct POINT {
    pub x: i32,
    pub y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RECT {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl RECT {
    pub fn width(&self) -> i32 {
        self.right - self.left
    }
    pub fn height(&self) -> i32 {
        self.bottom - self.top
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MSG {
    pub hwnd: HWND,
    pub message: u32,
    pub wParam: WPARAM,
    pub lParam: LPARAM,
    pub time: u32,
    pub pt: POINT,
    pub lPrivate: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PAINTSTRUCT {
    pub hdc: HDC,
    pub fErase: BOOL,
    pub rcPaint: RECT,
    pub fRestore: BOOL,
    pub fIncUpdate: BOOL,
    pub rgbReserved: [u8; 32],
}

#[repr(C)]
pub struct WNDCLASSW {
    pub style: UINT,
    pub lpfnWndProc: Option<unsafe extern "system" fn(HWND, UINT, WPARAM, LPARAM) -> LRESULT>,
    pub cbClsExtra: i32,
    pub cbWndExtra: i32,
    pub hInstance: HINSTANCE,
    pub hIcon: isize,
    pub hCursor: HCURSOR,
    pub hbrBackground: HBRUSH,
    pub lpszMenuName: *const u16,
    pub lpszClassName: *const u16,
}

#[repr(C)]
pub struct CREATESTRUCTW {
    pub lpCreateParams: *mut c_void,
    pub hInstance: HINSTANCE,
    pub hMenu: HMENU,
    pub hwndParent: HWND,
    pub cy: i32,
    pub cx: i32,
    pub y: i32,
    pub x: i32,
    pub style: i32,
    pub lpszName: *const u16,
    pub lpszClass: *const u16,
    pub dwExStyle: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct MINMAXINFO {
    pub ptReserved: POINT,
    pub ptMaxSize: POINT,
    pub ptMaxPosition: POINT,
    pub ptMinTrackSize: POINT,
    pub ptMaxTrackSize: POINT,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct INITCOMMONCONTROLSEX {
    pub dwSize: u32,
    pub dwICC: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NMHDR {
    pub hwndFrom: HWND,
    pub idFrom: usize,
    pub code: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct NMLISTVIEW {
    pub hdr: NMHDR,
    pub iItem: i32,
    pub iSubItem: i32,
    pub uNewState: u32,
    pub uOldState: u32,
    pub uChanged: u32,
    pub ptAction: POINT,
    pub lParam: LPARAM,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LVCOLUMNW {
    pub mask: UINT,
    pub fmt: i32,
    pub cx: i32,
    pub pszText: *mut u16,
    pub cchTextMax: i32,
    pub iSubItem: i32,
    pub iImage: i32,
    pub iOrder: i32,
    pub cxMin: i32,
    pub cxDefault: i32,
    pub cxIdeal: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct LVITEMW {
    pub mask: UINT,
    pub iItem: i32,
    pub iSubItem: i32,
    pub state: UINT,
    pub stateMask: UINT,
    pub pszText: *mut u16,
    pub cchTextMax: i32,
    pub iImage: i32,
    pub lParam: LPARAM,
    pub iIndent: i32,
    pub iGroupId: i32,
    pub cColumns: UINT,
    pub puColumns: *mut UINT,
    pub piColFmt: *mut i32,
    pub iGroup: i32,
}

impl LVCOLUMNW {
    pub fn zeroed() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

impl LVITEMW {
    pub fn zeroed() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

// ---------------------------------------------------------------------------
// 外部函数
// ---------------------------------------------------------------------------

#[link(name = "user32")]
unsafe extern "system" {
    pub fn RegisterClassW(lpWndClass: *const WNDCLASSW) -> ATOM;
    pub fn CreateWindowExW(
        dwExStyle: u32,
        lpClassName: *const u16,
        lpWindowName: *const u16,
        dwStyle: u32,
        X: i32,
        Y: i32,
        nWidth: i32,
        nHeight: i32,
        hWndParent: HWND,
        hMenu: HMENU,
        hInstance: HINSTANCE,
        lpParam: *mut c_void,
    ) -> HWND;
    pub fn DefWindowProcW(hWnd: HWND, Msg: UINT, wParam: WPARAM, lParam: LPARAM) -> LRESULT;
    pub fn DestroyWindow(hWnd: HWND) -> BOOL;
    pub fn ShowWindow(hWnd: HWND, nCmdShow: i32) -> BOOL;
    pub fn UpdateWindow(hWnd: HWND) -> BOOL;
    pub fn GetMessageW(lpMsg: *mut MSG, hWnd: HWND, wMsgFilterMin: u32, wMsgFilterMax: u32) -> BOOL;
    pub fn TranslateMessage(lpMsg: *const MSG) -> BOOL;
    pub fn DispatchMessageW(lpMsg: *const MSG) -> LRESULT;
    pub fn PostQuitMessage(nExitCode: i32);
    pub fn SendMessageW(hWnd: HWND, Msg: UINT, wParam: WPARAM, lParam: LPARAM) -> LRESULT;
    pub fn GetClientRect(hWnd: HWND, lpRect: *mut RECT) -> BOOL;
    pub fn MoveWindow(
        hWnd: HWND,
        X: i32,
        Y: i32,
        nWidth: i32,
        nHeight: i32,
        bRepaint: BOOL,
    ) -> BOOL;
    pub fn GetWindowTextW(hWnd: HWND, lpString: *mut u16, nMaxCount: i32) -> i32;
    pub fn SetWindowTextW(hWnd: HWND, lpString: *const u16) -> BOOL;
    pub fn MessageBoxW(hWnd: HWND, lpText: *const u16, lpCaption: *const u16, uType: u32) -> i32;
    pub fn InvalidateRect(hWnd: HWND, lpRect: *const RECT, bErase: BOOL) -> BOOL;
    pub fn SetTimer(hWnd: HWND, nIDEvent: usize, uElapse: u32, lpTimerFunc: *const c_void) -> usize;
    pub fn KillTimer(hWnd: HWND, uIDEvent: usize) -> BOOL;
    pub fn SetWindowLongPtrW(hWnd: HWND, nIndex: i32, dwNewLong: isize) -> isize;
    pub fn GetWindowLongPtrW(hWnd: HWND, nIndex: i32) -> isize;
    pub fn LoadCursorW(hInstance: HINSTANCE, lpCursorName: *const u16) -> HCURSOR;
    pub fn EnableWindow(hWnd: HWND, bEnable: BOOL) -> BOOL;
    pub fn SetFocus(hWnd: HWND) -> HWND;
    pub fn DrawTextW(
        hdc: HDC,
        lpchText: *const u16,
        cchText: i32,
        lprc: *mut RECT,
        format: u32,
    ) -> i32;
    pub fn FillRect(hdc: HDC, lprc: *const RECT, hbr: HBRUSH) -> i32;
    pub fn BeginPaint(hWnd: HWND, lpPaint: *mut PAINTSTRUCT) -> HDC;
    pub fn EndPaint(hWnd: HWND, lpPaint: *const PAINTSTRUCT) -> BOOL;
}

#[link(name = "gdi32")]
unsafe extern "system" {
    pub fn CreateSolidBrush(color: u32) -> HBRUSH;
    pub fn CreateFontW(
        cHeight: i32,
        cWidth: i32,
        cEscapement: i32,
        cOrientation: i32,
        cWeight: i32,
        bItalic: u32,
        bUnderline: u32,
        bStrikeOut: u32,
        iCharSet: u32,
        iOutPrecision: u32,
        iClipPrecision: u32,
        iQuality: u32,
        iPitchAndFamily: u32,
        pszFaceName: *const u16,
    ) -> HFONT;
    pub fn DeleteObject(ho: HGDIOBJ) -> BOOL;
    pub fn SelectObject(hdc: HDC, h: HGDIOBJ) -> HGDIOBJ;
    pub fn SetTextColor(hdc: HDC, color: u32) -> u32;
    pub fn SetBkMode(hdc: HDC, mode: i32) -> i32;
}

#[link(name = "comctl32")]
unsafe extern "system" {
    pub fn InitCommonControlsEx(picce: *const INITCOMMONCONTROLSEX) -> BOOL;
}

#[link(name = "kernel32")]
unsafe extern "system" {
    pub fn GetModuleHandleW(lpModuleName: *const u16) -> HINSTANCE;
}

// ---------------------------------------------------------------------------
// 辅助函数
// ---------------------------------------------------------------------------

/// 转成以 0 结尾的宽字符串。
pub fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 创建界面字体。
pub fn make_font(size: i32, weight: i32) -> HFONT {
    unsafe {
        CreateFontW(
            size,
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            0,
            0,
            0,
            0,
            w("Microsoft YaHei UI").as_ptr(),
        )
    }
}

pub unsafe fn set_font(hwnd: HWND, font: HFONT) {
    SendMessageW(hwnd, WM_SETFONT, font as WPARAM, 1);
}

/// 向 ListView 添加一列。
pub unsafe fn lv_add_column(lv: HWND, index: i32, title: &str, width: i32, right_align: bool) {
    let mut text = w(title);
    let mut col = LVCOLUMNW::zeroed();
    col.mask = LVCF_FMT | LVCF_WIDTH | LVCF_TEXT | LVCF_SUBITEM;
    col.fmt = if right_align { LVCFMT_RIGHT } else { LVCFMT_LEFT };
    col.cx = width;
    col.pszText = text.as_mut_ptr();
    col.cchTextMax = 0;
    col.iSubItem = index;
    SendMessageW(
        lv,
        LVM_INSERTCOLUMNW,
        index as WPARAM,
        &mut col as *mut LVCOLUMNW as LPARAM,
    );
}

/// 向 ListView 追加一行。
pub unsafe fn lv_add_row(lv: HWND, row: i32, cells: &[String]) {
    for (col, cell) in cells.iter().enumerate() {
        let mut text = w(cell);
        let mut item = LVITEMW::zeroed();
        item.mask = LVIF_TEXT;
        item.iItem = row;
        item.iSubItem = col as i32;
        item.pszText = text.as_mut_ptr();
        if col == 0 {
            SendMessageW(lv, LVM_INSERTITEMW, 0, &mut item as *mut LVITEMW as LPARAM);
        } else {
            SendMessageW(
                lv,
                LVM_SETITEMTEXTW,
                row as WPARAM,
                &mut item as *mut LVITEMW as LPARAM,
            );
        }
    }
}

pub unsafe fn lv_clear(lv: HWND) {
    SendMessageW(lv, LVM_DELETEALLITEMS, 0, 0);
}

/// 获取所有选中行（0 基索引）。
pub unsafe fn lv_selected_rows(lv: HWND) -> Vec<i32> {
    let mut out = Vec::new();
    let mut idx = SendMessageW(lv, LVM_GETNEXTITEM, usize::MAX, LVNI_SELECTED as LPARAM) as i32;
    while idx != -1 {
        out.push(idx);
        idx = SendMessageW(lv, LVM_GETNEXTITEM, idx as WPARAM, LVNI_SELECTED as LPARAM) as i32;
    }
    out
}

pub unsafe fn lv_select_row(lv: HWND, row: i32, selected: bool) {
    let mut item = LVITEMW::zeroed();
    item.stateMask = LVIS_SELECTED | LVIS_FOCUSED;
    item.state = if selected {
        LVIS_SELECTED | LVIS_FOCUSED
    } else {
        0
    };
    SendMessageW(
        lv,
        LVM_SETITEMSTATE,
        row as WPARAM,
        &mut item as *mut LVITEMW as LPARAM,
    );
}

pub unsafe fn lv_clear_selection(lv: HWND) {
    let count = SendMessageW(lv, LVM_GETITEMCOUNT, 0, 0) as i32;
    for i in 0..count {
        lv_select_row(lv, i, false);
    }
}

/// 取控件文本。
pub unsafe fn get_text(hwnd: HWND) -> String {
    let mut buf = vec![0u16; 512];
    let len = GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// 在指定矩形内绘制文本。
pub unsafe fn draw_text(hdc: HDC, s: &str, mut rc: RECT, format: u32, color: u32, font: HFONT) {
    let old = SelectObject(hdc, font as HGDIOBJ);
    SetTextColor(hdc, color);
    SetBkMode(hdc, TRANSPARENT);
    let buf: Vec<u16> = s.encode_utf16().collect();
    DrawTextW(
        hdc,
        buf.as_ptr(),
        buf.len() as i32,
        &mut rc,
        format,
    );
    SelectObject(hdc, old);
}

/// 填充矩形。
pub unsafe fn fill_rect(hdc: HDC, rc: &RECT, color: u32) {
    let brush = CreateSolidBrush(color);
    FillRect(hdc, rc, brush);
    DeleteObject(brush);
}
