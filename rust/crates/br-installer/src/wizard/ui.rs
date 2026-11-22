// SPDX-License-Identifier: AGPL-3.0-only
// Copyright (c) BetterRecall contributors

//! Win32 Aero Wizard (`PropertySheetW` with `PSH_WIZARD | PSH_AEROWIZARD`).

use std::ffi::{OsString, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use br_fetch::Catalog;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    BST_CHECKED, BST_UNCHECKED, ICC_PROGRESS_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX,
    InitCommonControlsEx, NMHDR, PBM_SETMARQUEE, PBM_SETPOS, PBM_SETRANGE32, PBS_MARQUEE,
    PROPSHEETHEADERW_V2, PROPSHEETHEADERW_V2_0, PROPSHEETHEADERW_V2_2, PROPSHEETPAGEW,
    PROPSHEETPAGEW_0, PSH_AEROWIZARD, PSH_NOAPPLYNOW, PSH_PROPSHEETPAGE, PSH_USEICONID,
    PSH_WIZARD, PSM_SETBUTTONTEXTW, PSM_SETCURSELID, PSM_SETHEADERTITLEW, PSM_SETWIZBUTTONS,
    PSN_QUERYCANCEL, PSN_SETACTIVE, PSP_DEFAULT, PSP_USEHEADERTITLE, PSWIZB_BACK, PSWIZB_FINISH,
    PSWIZB_NEXT, PropertySheetW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    BM_GETCHECK, BM_SETCHECK, BN_CLICKED, DWLP_MSGRESULT, GWL_STYLE, GetDlgItem, GetParent,
    GetWindowLongPtrW, GetWindowLongW, IDCANCEL, PostMessageW, SW_HIDE, SW_SHOW, SendMessageW,
    SetDlgItemTextW, SetWindowLongPtrW, SetWindowLongW, ShowWindow, WINDOW_LONG_PTR_INDEX, WM_APP,
    WM_COMMAND, WM_INITDIALOG, WM_NOTIFY,
};
use windows::core::PCWSTR;

use crate::cli::{Action, CliArgs, load_dev_catalog};
use crate::ctx::Ctx;
use crate::error::{InstallError, exit_code};
use crate::extract::available_disk_space;
use crate::install::{
    InstallOpts, ModelMode, ModelsResult, ModifyOpts, Outcome, UninstallOpts,
    install_with_progress, modify_with_progress, resolve_models_dir, uninstall_with_progress,
};
use crate::payload::read_license_text;
use crate::progress::{Progress, Step};
use crate::registry::read_model_mode;
use crate::strings;
use crate::wizard::ids::{
    IDC_FINISH_DETAIL, IDC_FINISH_SUMMARY, IDC_LICENCE_ACCEPT, IDC_LICENCE_EDIT,
    IDC_LICENCE_THIRD_PARTY, IDC_LICENCE_WELCOME, IDC_MODELS_FULL, IDC_MODELS_FULL_DESC,
    IDC_MODELS_LATER, IDC_MODELS_LATER_DESC, IDC_MODELS_MINIMAL, IDC_MODELS_MINIMAL_DESC,
    IDC_MODELS_NOTE, IDC_PROGRESS_BAR, IDC_PROGRESS_DETAIL, IDC_PROGRESS_STATUS,
    IDC_READY_DATA_LABEL, IDC_READY_DATA_PATH, IDC_READY_DOWNLOAD_INFO, IDC_READY_INSTALL_LABEL,
    IDC_READY_INSTALL_PATH, IDC_READY_MODELS_LABEL, IDC_READY_MODELS_PATH, IDC_READY_START_CHECK,
    IDC_UNINSTALL_DELETE, IDC_UNINSTALL_KEEP, IDC_UNINSTALL_UNDERSTAND, IDC_UNINSTALL_WARNING,
    IDD_FINISH, IDI_APP_ICON,
};
use crate::wizard::model::{
    FinishMessage, PageId, WizardButtons, WizardInit, WizardModel, format_finish_error,
    format_finish_outcome, model_mode_from_radio, pages_for_action, radio_from_model_mode,
    scale_progress,
};

const WM_APP_STEP: u32 = WM_APP + 1;
const WM_APP_BYTES: u32 = WM_APP + 2;
const WM_APP_DONE: u32 = WM_APP + 3;

// On 64-bit Windows, DWLP_MSGRESULT = 0, DWLP_DLGPROC = 8, DWLP_USER = 16.
const DWLP_MSGRESULT_IDX: WINDOW_LONG_PTR_INDEX = WINDOW_LONG_PTR_INDEX(DWLP_MSGRESULT as i32);
const DWLP_USER_IDX: WINDOW_LONG_PTR_INDEX =
    WINDOW_LONG_PTR_INDEX((std::mem::size_of::<isize>() * 2) as i32);

struct WizardState {
    ctx: Ctx,
    exe: PathBuf,
    allow_downgrade: bool,
    licence_text: String,
    catalog: Option<Catalog>,
    dev_catalog: Option<Catalog>,
    model: WizardModel,
    cancel_flag: Arc<AtomicBool>,
    bytes_done: Arc<AtomicU64>,
    bytes_total: Arc<AtomicU64>,
    download_start: Option<Instant>,
    worker_started: bool,
    worker_handle: Option<JoinHandle<()>>,
    finish_message: Option<FinishMessage>,
    exit_code: i32,
}

struct WizardProgress {
    hwnd: isize,
    cancel_flag: Arc<AtomicBool>,
    bytes_done: Arc<AtomicU64>,
    bytes_total: Arc<AtomicU64>,
}

impl Progress for WizardProgress {
    fn step(&self, step: Step) {
        let hwnd = HWND(self.hwnd as *mut c_void);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_STEP, WPARAM(step as usize), LPARAM(0));
        }
    }

    fn bytes(&self, done: u64, total: u64) {
        self.bytes_done.store(done, Ordering::Relaxed);
        self.bytes_total.store(total, Ordering::Relaxed);
        let pos = scale_progress(done, total);
        let hwnd = HWND(self.hwnd as *mut c_void);
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_APP_BYTES, WPARAM(pos as usize), LPARAM(0));
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel_flag.load(Ordering::SeqCst)
    }
}

/// Runs the interactive Windows Aero Wizard and returns the process exit code.
pub fn run_wizard(ctx: Ctx, args: CliArgs) -> i32 {
    let icc = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_PROGRESS_CLASS | ICC_STANDARD_CLASSES,
    };
    unsafe {
        let _ = InitCommonControlsEx(&icc);
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(_) => return exit_code::OTHER,
    };

    let (licence_found, licence_text) = if args.action == Action::Install {
        match read_license_text(&exe) {
            Ok(Some(text)) => (true, text),
            Ok(None) | Err(_) => (false, strings::LICENCE_MISSING.to_string()),
        }
    } else {
        (true, String::new())
    };

    let existing_model_mode = read_model_mode(&ctx);
    let model = WizardModel::new(WizardInit {
        action: args.action,
        existing_model_mode,
        cli_model_mode: args.model_mode,
        cli_delete_data: args.delete_data,
        // A dev or test root never starts the app (`Ctx::from_dev_root` sets `start_after` false).
        cli_no_run: args.no_run || !ctx.start_after,
        licence_found,
    });

    let dev_catalog = load_dev_catalog(&args).ok().flatten();
    let catalog = dev_catalog.clone().or_else(|| Catalog::embedded().ok());

    let mut state = Box::new(WizardState {
        ctx,
        exe,
        allow_downgrade: args.allow_downgrade,
        licence_text,
        catalog,
        dev_catalog,
        model,
        cancel_flag: Arc::new(AtomicBool::new(false)),
        bytes_done: Arc::new(AtomicU64::new(0)),
        bytes_total: Arc::new(AtomicU64::new(0)),
        download_start: None,
        worker_started: false,
        worker_handle: None,
        finish_message: None,
        exit_code: exit_code::CANCELLED,
    });

    let hinstance = match unsafe { GetModuleHandleW(None) } {
        Ok(m) => HINSTANCE::from(m),
        Err(_) => return exit_code::OTHER,
    };

    let page_ids = pages_for_action(args.action);
    let header_wides: Vec<Vec<u16>> = page_ids
        .iter()
        .map(|p| to_wide_nul(p.header_title(args.action)))
        .collect();
    let state_ptr = (&mut *state as *mut WizardState) as isize;

    let mut pages: Vec<PROPSHEETPAGEW> = page_ids
        .iter()
        .zip(header_wides.iter())
        .map(|(page_id, title_w)| PROPSHEETPAGEW {
            dwSize: std::mem::size_of::<PROPSHEETPAGEW>() as u32,
            dwFlags: PSP_DEFAULT | PSP_USEHEADERTITLE,
            hInstance: hinstance,
            Anonymous1: PROPSHEETPAGEW_0 {
                pszTemplate: make_int_resource(page_id.dialog_id()),
            },
            pfnDlgProc: Some(page_proc_for(*page_id)),
            lParam: LPARAM(state_ptr),
            pszHeaderTitle: PCWSTR(title_w.as_ptr()),
            ..Default::default()
        })
        .collect();

    let caption_w = to_wide_nul(strings::WINDOW_TITLE);
    let mut psh = PROPSHEETHEADERW_V2 {
        dwSize: std::mem::size_of::<PROPSHEETHEADERW_V2>() as u32,
        dwFlags: PSH_WIZARD
            | PSH_AEROWIZARD
            | PSH_PROPSHEETPAGE
            | PSH_USEICONID
            | PSH_NOAPPLYNOW,
        hwndParent: HWND::default(),
        hInstance: hinstance,
        Anonymous1: PROPSHEETHEADERW_V2_0 {
            pszIcon: make_int_resource(IDI_APP_ICON),
        },
        pszCaption: PCWSTR(caption_w.as_ptr()),
        nPages: pages.len() as u32,
        Anonymous3: PROPSHEETHEADERW_V2_2 {
            ppsp: pages.as_mut_ptr(),
        },
        ..Default::default()
    };

    unsafe {
        let _ = PropertySheetW(&mut psh);
    }

    if let Some(handle) = state.worker_handle.take() {
        let _ = handle.join();
    }

    state.exit_code
}

type DlgProcFn = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> isize;

fn page_proc_for(page: PageId) -> DlgProcFn {
    match page {
        PageId::Licence => licence_proc,
        PageId::Models => models_proc,
        PageId::Ready => ready_proc,
        PageId::UninstallConfirm => uninstall_confirm_proc,
        PageId::Progress => progress_proc,
        PageId::Finish => finish_proc,
    }
}

unsafe extern "system" fn licence_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => {
            let Some(state) = (unsafe { init_page_state(hwnd, lparam) }) else {
                return 0;
            };
            set_item_text(hwnd, IDC_LICENCE_WELCOME, strings::LICENCE_WELCOME);
            set_item_text(hwnd, IDC_LICENCE_EDIT, &state.licence_text);
            set_item_text(hwnd, IDC_LICENCE_THIRD_PARTY, strings::LICENCE_THIRD_PARTY);
            set_item_text(hwnd, IDC_LICENCE_ACCEPT, strings::LICENCE_ACCEPT);
            set_checked(hwnd, IDC_LICENCE_ACCEPT, state.model.licence_accepted);
            if !state.model.licence_found {
                enable_item(hwnd, IDC_LICENCE_ACCEPT, false);
            }
            1
        }
        WM_COMMAND => {
            let id = loword(wparam.0);
            let code = hiword(wparam.0);
            if id == IDC_LICENCE_ACCEPT && code == BN_CLICKED as u16 {
                if let Some(state) = unsafe { page_state(hwnd) } {
                    state.model.licence_accepted = is_checked(hwnd, IDC_LICENCE_ACCEPT);
                    apply_buttons(hwnd, state.model.buttons_for(PageId::Licence));
                }
                return 1;
            }
            0
        }
        WM_NOTIFY => {
            if notify_code(lparam) == PSN_SETACTIVE {
                if let Some(state) = unsafe { page_state(hwnd) } {
                    apply_buttons(hwnd, state.model.buttons_for(PageId::Licence));
                }
                return 1;
            }
            0
        }
        _ => 0,
    }
}

unsafe extern "system" fn models_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => {
            let Some(state) = (unsafe { init_page_state(hwnd, lparam) }) else {
                return 0;
            };
            let cat = state.catalog.as_ref();
            let full_label = strings::model_mode_radio_label(ModelMode::Full, cat);
            let minimal_label = strings::model_mode_radio_label(ModelMode::Minimal, cat);
            let later_label = strings::model_mode_radio_label(ModelMode::Later, cat);
            set_item_text(hwnd, IDC_MODELS_FULL, &full_label);
            set_item_text(
                hwnd,
                IDC_MODELS_FULL_DESC,
                strings::model_mode_description(ModelMode::Full),
            );
            set_item_text(hwnd, IDC_MODELS_MINIMAL, &minimal_label);
            set_item_text(
                hwnd,
                IDC_MODELS_MINIMAL_DESC,
                strings::model_mode_description(ModelMode::Minimal),
            );
            set_item_text(hwnd, IDC_MODELS_LATER, &later_label);
            set_item_text(
                hwnd,
                IDC_MODELS_LATER_DESC,
                strings::model_mode_description(ModelMode::Later),
            );
            set_item_text(hwnd, IDC_MODELS_NOTE, strings::MODELS_NOTE);

            let selected_id = radio_from_model_mode(state.model.model_mode);
            set_checked(hwnd, selected_id, true);
            1
        }
        WM_COMMAND => {
            let id = loword(wparam.0);
            let code = hiword(wparam.0);
            if code == BN_CLICKED as u16
                && let Some(mode) = model_mode_from_radio(id)
                && let Some(state) = (unsafe { page_state(hwnd) })
            {
                state.model.model_mode = mode;
                return 1;
            }
            0
        }
        WM_NOTIFY => {
            if notify_code(lparam) == PSN_SETACTIVE {
                if let Some(state) = unsafe { page_state(hwnd) } {
                    apply_buttons(hwnd, state.model.buttons_for(PageId::Models));
                }
                return 1;
            }
            0
        }
        _ => 0,
    }
}

unsafe extern "system" fn ready_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => {
            let Some(state) = (unsafe { init_page_state(hwnd, lparam) }) else {
                return 0;
            };
            set_item_text(hwnd, IDC_READY_INSTALL_LABEL, strings::READY_INSTALL_LABEL);
            set_item_text(
                hwnd,
                IDC_READY_INSTALL_PATH,
                &state.ctx.install_dir.to_string_lossy(),
            );
            set_item_text(hwnd, IDC_READY_DATA_LABEL, strings::READY_DATA_LABEL);
            set_item_text(hwnd, IDC_READY_DATA_PATH, &state.ctx.home.to_string_lossy());
            set_item_text(hwnd, IDC_READY_MODELS_LABEL, strings::READY_MODELS_LABEL);
            let models_dir = resolve_models_dir(&state.ctx.home);
            set_item_text(hwnd, IDC_READY_MODELS_PATH, &models_dir.to_string_lossy());
            let dl_summary =
                strings::ready_models_summary(state.model.model_mode, state.catalog.as_ref());
            set_item_text(hwnd, IDC_READY_DOWNLOAD_INFO, &dl_summary);
            set_item_text(hwnd, IDC_READY_START_CHECK, strings::READY_START_CHECK);
            set_checked(hwnd, IDC_READY_START_CHECK, state.model.start_after);
            1
        }
        WM_COMMAND => {
            let id = loword(wparam.0);
            let code = hiword(wparam.0);
            if id == IDC_READY_START_CHECK && code == BN_CLICKED as u16 {
                if let Some(state) = unsafe { page_state(hwnd) } {
                    state.model.start_after = is_checked(hwnd, IDC_READY_START_CHECK);
                }
                return 1;
            }
            0
        }
        WM_NOTIFY => {
            if notify_code(lparam) == PSN_SETACTIVE {
                if let Some(state) = unsafe { page_state(hwnd) } {
                    let dl_summary = strings::ready_models_summary(
                        state.model.model_mode,
                        state.catalog.as_ref(),
                    );
                    set_item_text(hwnd, IDC_READY_DOWNLOAD_INFO, &dl_summary);
                    apply_buttons(hwnd, state.model.buttons_for(PageId::Ready));
                }
                return 1;
            }
            0
        }
        _ => 0,
    }
}

unsafe extern "system" fn uninstall_confirm_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => {
            let Some(state) = (unsafe { init_page_state(hwnd, lparam) }) else {
                return 0;
            };
            let keep_label = strings::uninstall_keep_label(&state.ctx.home);
            set_item_text(hwnd, IDC_UNINSTALL_KEEP, &keep_label);
            set_item_text(hwnd, IDC_UNINSTALL_DELETE, strings::UNINSTALL_DELETE);
            set_item_text(hwnd, IDC_UNINSTALL_WARNING, strings::UNINSTALL_WARNING);
            set_item_text(
                hwnd,
                IDC_UNINSTALL_UNDERSTAND,
                strings::UNINSTALL_UNDERSTAND,
            );

            set_checked(hwnd, IDC_UNINSTALL_KEEP, !state.model.delete_data);
            set_checked(hwnd, IDC_UNINSTALL_DELETE, state.model.delete_data);
            set_checked(hwnd, IDC_UNINSTALL_UNDERSTAND, state.model.delete_understood);
            sync_delete_warning_visibility(hwnd, state.model.show_delete_warning());
            1
        }
        WM_COMMAND => {
            let id = loword(wparam.0);
            let code = hiword(wparam.0);
            if code == BN_CLICKED as u16
                && let Some(state) = (unsafe { page_state(hwnd) })
            {
                match id {
                    IDC_UNINSTALL_KEEP => {
                        state.model.delete_data = false;
                        sync_delete_warning_visibility(hwnd, false);
                        apply_buttons(hwnd, state.model.buttons_for(PageId::UninstallConfirm));
                        return 1;
                    }
                    IDC_UNINSTALL_DELETE => {
                        state.model.delete_data = true;
                        sync_delete_warning_visibility(hwnd, true);
                        apply_buttons(hwnd, state.model.buttons_for(PageId::UninstallConfirm));
                        return 1;
                    }
                    IDC_UNINSTALL_UNDERSTAND => {
                        state.model.delete_understood = is_checked(hwnd, IDC_UNINSTALL_UNDERSTAND);
                        apply_buttons(hwnd, state.model.buttons_for(PageId::UninstallConfirm));
                        return 1;
                    }
                    _ => {}
                }
            }
            0
        }
        WM_NOTIFY => {
            if notify_code(lparam) == PSN_SETACTIVE {
                if let Some(state) = unsafe { page_state(hwnd) } {
                    apply_buttons(hwnd, state.model.buttons_for(PageId::UninstallConfirm));
                }
                return 1;
            }
            0
        }
        _ => 0,
    }
}

unsafe extern "system" fn progress_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => {
            let Some(_) = (unsafe { init_page_state(hwnd, lparam) }) else {
                return 0;
            };
            if let Ok(bar) = unsafe { GetDlgItem(Some(hwnd), i32::from(IDC_PROGRESS_BAR)) } {
                unsafe {
                    SendMessageW(
                        bar,
                        PBM_SETRANGE32,
                        Some(WPARAM(0)),
                        Some(LPARAM(1000)),
                    );
                }
            }
            1
        }
        WM_NOTIFY => {
            let code = notify_code(lparam);
            if code == PSN_SETACTIVE {
                if let Some(state) = unsafe { page_state(hwnd) } {
                    apply_buttons(hwnd, state.model.buttons_for(PageId::Progress));
                    if !state.worker_started {
                        state.worker_started = true;
                        set_progress_marquee(hwnd, true);
                        spawn_worker(hwnd, state);
                    }
                }
                return 1;
            }
            if code == PSN_QUERYCANCEL {
                if let Some(state) = unsafe { page_state(hwnd) } {
                    if state.model.can_cancel() {
                        state.cancel_flag.store(true, Ordering::SeqCst);
                        set_item_text(hwnd, IDC_PROGRESS_STATUS, strings::PROGRESS_CANCELLING);
                        set_sheet_cancel_enabled(hwnd, false);
                    }
                    // Always block immediate sheet close on Progress; the worker posts WM_APP_DONE
                    // once cleanup finishes and advances to the Finish page.
                    unsafe {
                        SetWindowLongPtrW(hwnd, DWLP_MSGRESULT_IDX, 1);
                    }
                    return 1;
                }
                return 0;
            }
            0
        }
        WM_APP_STEP => {
            if let Some(step) = Step::from_u32(wparam.0 as u32)
                && let Some(state) = (unsafe { page_state(hwnd) })
            {
                state.model.on_step(step);
                set_item_text(hwnd, IDC_PROGRESS_STATUS, strings::step_label(step));
                if step == Step::DownloadingModels {
                    state.download_start = Some(Instant::now());
                    set_progress_marquee(hwnd, false);
                } else if step == Step::Extracting {
                    state.download_start = None;
                    set_item_text(hwnd, IDC_PROGRESS_DETAIL, "");
                    set_progress_marquee(hwnd, false);
                } else if step != Step::Done {
                    state.download_start = None;
                    set_item_text(hwnd, IDC_PROGRESS_DETAIL, "");
                    set_progress_marquee(hwnd, true);
                } else {
                    set_item_text(hwnd, IDC_PROGRESS_DETAIL, "");
                }
                apply_buttons(hwnd, state.model.buttons_for(PageId::Progress));
            }
            1
        }
        WM_APP_BYTES => {
            set_progress_marquee(hwnd, false);
            if let Ok(bar) = unsafe { GetDlgItem(Some(hwnd), i32::from(IDC_PROGRESS_BAR)) } {
                unsafe {
                    SendMessageW(bar, PBM_SETPOS, Some(WPARAM(wparam.0)), Some(LPARAM(0)));
                }
            }
            if let Some(state) = unsafe { page_state(hwnd) }
                && state.model.current_step == Some(Step::DownloadingModels)
            {
                let done = state.bytes_done.load(Ordering::Relaxed);
                let total = state.bytes_total.load(Ordering::Relaxed);
                let elapsed = state
                    .download_start
                    .map_or_else(std::time::Duration::default, |t| t.elapsed());
                let detail = strings::format_download_progress(done, total, elapsed);
                set_item_text(hwnd, IDC_PROGRESS_DETAIL, &detail);
            }
            1
        }
        WM_APP_DONE => {
            let boxed = unsafe { Box::from_raw(lparam.0 as *mut Result<Outcome, InstallError>) };
            if let Some(state) = unsafe { page_state(hwnd) } {
                state.model.work_done = true;
                match *boxed {
                    Ok(ref outcome) => {
                        state.exit_code = match outcome {
                            Outcome::Installed {
                                models: ModelsResult::Failed(_),
                                ..
                            }
                            | Outcome::Modified {
                                models: ModelsResult::Failed(_),
                                ..
                            } => exit_code::MODELS,
                            _ => exit_code::OK,
                        };
                        state.finish_message =
                            Some(format_finish_outcome(state.model.action, outcome));
                    }
                    Err(ref err) => {
                        state.exit_code = err.exit_code();
                        state.finish_message = Some(format_finish_error(err));
                    }
                }
                if let Ok(sheet) = unsafe { GetParent(hwnd) } {
                    unsafe {
                        let _ = PostMessageW(
                            Some(sheet),
                            PSM_SETCURSELID,
                            WPARAM(0),
                            LPARAM(IDD_FINISH as isize),
                        );
                    }
                }
            }
            1
        }
        _ => 0,
    }
}

unsafe extern "system" fn finish_proc(
    hwnd: HWND,
    msg: u32,
    _wparam: WPARAM,
    lparam: LPARAM,
) -> isize {
    match msg {
        WM_INITDIALOG => {
            let _ = unsafe { init_page_state(hwnd, lparam) };
            1
        }
        WM_NOTIFY => {
            let code = notify_code(lparam);
            if code == PSN_SETACTIVE {
                if let Some(state) = unsafe { page_state(hwnd) } {
                    if let Some(ref msg) = state.finish_message {
                        set_item_text(hwnd, IDC_FINISH_SUMMARY, &msg.summary);
                        set_item_text(hwnd, IDC_FINISH_DETAIL, &msg.detail);
                        set_finish_header(hwnd, state.model.action, msg.header);
                    }
                    apply_buttons(hwnd, state.model.buttons_for(PageId::Finish));
                }
                return 1;
            }
            0
        }
        _ => 0,
    }
}

fn spawn_worker(hwnd: HWND, state: &mut WizardState) {
    let mut ctx = state.ctx.clone();
    ctx.start_after = state.ctx.start_after && state.model.start_after;
    let exe = state.exe.clone();
    let action = state.model.action;
    let allow_downgrade = state.allow_downgrade;
    let model_mode = state.model.model_mode;
    let delete_data = state.model.delete_data;
    let hwnd_raw = hwnd.0 as isize;
    let cancel_flag = Arc::clone(&state.cancel_flag);
    let bytes_done = Arc::clone(&state.bytes_done);
    let bytes_total = Arc::clone(&state.bytes_total);
    let dev_catalog = state.dev_catalog.clone();

    let handle = thread::spawn(move || {
        let progress = WizardProgress {
            hwnd: hwnd_raw,
            cancel_flag,
            bytes_done,
            bytes_total,
        };
        let result = match action {
            Action::Install => install_with_progress(
                &ctx,
                &exe,
                &InstallOpts {
                    allow_downgrade,
                    model_mode,
                    catalog: dev_catalog,
                    integrity_key: None,
                },
                available_disk_space,
                &progress,
            ),
            Action::Modify => modify_with_progress(
                &ctx,
                &ModifyOpts {
                    model_mode,
                    catalog: dev_catalog,
                    integrity_key: None,
                },
                &progress,
            ),
            Action::Uninstall => uninstall_with_progress(
                &ctx,
                &UninstallOpts {
                    delete_data,
                    integrity_key: None,
                },
                &progress,
            ),
        };
        let ptr = Box::into_raw(Box::new(result));
        let target = HWND(hwnd_raw as *mut c_void);
        unsafe {
            let _ = PostMessageW(Some(target), WM_APP_DONE, WPARAM(0), LPARAM(ptr as isize));
        }
    });
    state.worker_handle = Some(handle);
}

fn set_finish_header(hwnd: HWND, action: Action, header: &str) {
    let Ok(sheet) = (unsafe { GetParent(hwnd) }) else {
        return;
    };
    let page_index = pages_for_action(action).len().saturating_sub(1);
    let wide = to_wide_nul(header);
    unsafe {
        SendMessageW(
            sheet,
            PSM_SETHEADERTITLEW,
            Some(WPARAM(page_index)),
            Some(LPARAM(wide.as_ptr() as isize)),
        );
    }
}

fn apply_buttons(hwnd: HWND, buttons: WizardButtons) {
    let Ok(sheet) = (unsafe { GetParent(hwnd) }) else {
        return;
    };
    let mut flags = 0u32;
    if buttons.back {
        flags |= PSWIZB_BACK;
    }
    if buttons.next {
        flags |= PSWIZB_NEXT;
    }
    if buttons.finish {
        flags |= PSWIZB_FINISH;
    }
    let next_label = buttons.next_label.unwrap_or(strings::BTN_NEXT);
    let next_wide = to_wide_nul(next_label);
    unsafe {
        SendMessageW(
            sheet,
            PSM_SETBUTTONTEXTW,
            Some(WPARAM(PSWIZB_NEXT as usize)),
            Some(LPARAM(next_wide.as_ptr() as isize)),
        );
        let _ = PostMessageW(
            Some(sheet),
            PSM_SETWIZBUTTONS,
            WPARAM(0),
            LPARAM(flags as isize),
        );
    }
    set_sheet_cancel_enabled(hwnd, buttons.cancel);
}

fn set_sheet_cancel_enabled(hwnd: HWND, enabled: bool) {
    if let Ok(sheet) = (unsafe { GetParent(hwnd) })
        && let Ok(cancel_btn) = (unsafe { GetDlgItem(Some(sheet), IDCANCEL.0) })
    {
        unsafe {
            let _ = EnableWindow(cancel_btn, enabled);
        }
    }
}

fn set_progress_marquee(hwnd: HWND, enabled: bool) {
    let Ok(bar) = (unsafe { GetDlgItem(Some(hwnd), i32::from(IDC_PROGRESS_BAR)) }) else {
        return;
    };
    unsafe {
        let style = GetWindowLongW(bar, GWL_STYLE) as u32;
        let new_style = if enabled {
            style | PBS_MARQUEE
        } else {
            style & !PBS_MARQUEE
        };
        if new_style != style {
            SetWindowLongW(bar, GWL_STYLE, new_style as i32);
        }
        SendMessageW(
            bar,
            PBM_SETMARQUEE,
            Some(WPARAM(usize::from(enabled))),
            Some(LPARAM(if enabled { 30 } else { 0 })),
        );
    }
}

fn sync_delete_warning_visibility(hwnd: HWND, visible: bool) {
    let cmd = if visible { SW_SHOW } else { SW_HIDE };
    for id in [IDC_UNINSTALL_WARNING, IDC_UNINSTALL_UNDERSTAND] {
        if let Ok(ctrl) = unsafe { GetDlgItem(Some(hwnd), i32::from(id)) } {
            unsafe {
                let _ = ShowWindow(ctrl, cmd);
            }
        }
    }
}

unsafe fn init_page_state<'a>(hwnd: HWND, lparam: LPARAM) -> Option<&'a mut WizardState> {
    let psp = lparam.0 as *const PROPSHEETPAGEW;
    if psp.is_null() {
        return None;
    }
    let ptr = unsafe { (*psp).lParam.0 };
    unsafe {
        SetWindowLongPtrW(hwnd, DWLP_USER_IDX, ptr);
    }
    unsafe { (ptr as *mut WizardState).as_mut() }
}

unsafe fn page_state<'a>(hwnd: HWND) -> Option<&'a mut WizardState> {
    let ptr = unsafe { GetWindowLongPtrW(hwnd, DWLP_USER_IDX) } as *mut WizardState;
    unsafe { ptr.as_mut() }
}

fn notify_code(lparam: LPARAM) -> u32 {
    let hdr = lparam.0 as *const NMHDR;
    if hdr.is_null() {
        0
    } else {
        unsafe { (*hdr).code }
    }
}

fn set_item_text(hwnd: HWND, id: u16, text: &str) {
    let wide = to_wide_nul(text);
    unsafe {
        let _ = SetDlgItemTextW(hwnd, i32::from(id), PCWSTR(wide.as_ptr()));
    }
}

fn set_checked(hwnd: HWND, id: u16, checked: bool) {
    if let Ok(ctrl) = unsafe { GetDlgItem(Some(hwnd), i32::from(id)) } {
        let state = if checked { BST_CHECKED } else { BST_UNCHECKED };
        unsafe {
            SendMessageW(ctrl, BM_SETCHECK, Some(WPARAM(state.0 as usize)), Some(LPARAM(0)));
        }
    }
}

fn is_checked(hwnd: HWND, id: u16) -> bool {
    let Ok(ctrl) = (unsafe { GetDlgItem(Some(hwnd), i32::from(id)) }) else {
        return false;
    };
    let res = unsafe { SendMessageW(ctrl, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0))) };
    res.0 as u32 == BST_CHECKED.0
}

fn enable_item(hwnd: HWND, id: u16, enabled: bool) {
    if let Ok(ctrl) = unsafe { GetDlgItem(Some(hwnd), i32::from(id)) } {
        unsafe {
            let _ = EnableWindow(ctrl, enabled);
        }
    }
}

fn make_int_resource(id: u16) -> PCWSTR {
    PCWSTR(usize::from(id) as *const u16)
}

fn loword(v: usize) -> u16 {
    (v & 0xFFFF) as u16
}

fn hiword(v: usize) -> u16 {
    ((v >> 16) & 0xFFFF) as u16
}

fn to_wide_nul(s: &str) -> Vec<u16> {
    OsString::from(s).encode_wide().chain(Some(0)).collect()
}
