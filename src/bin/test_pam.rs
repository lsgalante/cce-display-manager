use pam_sys;
use libc;
use std::ffi::{CString, CStr};
use std::ptr;

struct PamSessionData {
    username: String,
    password: String,
}

extern "C" fn pam_conversation_fn(
    num_msg: libc::c_int,
    msg: *mut *mut pam_sys::PamMessage,
    out_resp: *mut *mut pam_sys::PamResponse,
    appdata_ptr: *mut libc::c_void,
) -> libc::c_int {
    let data = unsafe { &*(appdata_ptr as *const PamSessionData) };
    let resp_size = std::mem::size_of::<pam_sys::PamResponse>();
    let resp = unsafe { libc::calloc(num_msg as usize, resp_size) as *mut pam_sys::PamResponse };
    if resp.is_null() {
        return pam_sys::PamReturnCode::BUF_ERR as libc::c_int;
    }

    for i in 0..num_msg as isize {
        unsafe {
            let m = &**msg.offset(i);
            let r = &mut *resp.offset(i);
            let style = m.msg_style;
            if style == pam_sys::PamMessageStyle::PROMPT_ECHO_ON as libc::c_int {
                let user_c = CString::new(data.username.clone()).unwrap();
                r.resp = libc::strdup(user_c.as_ptr());
            } else if style == pam_sys::PamMessageStyle::PROMPT_ECHO_OFF as libc::c_int {
                let pass_c = CString::new(data.password.clone()).unwrap();
                r.resp = libc::strdup(pass_c.as_ptr());
            }
        }
    }

    unsafe { *out_resp = resp };
    pam_sys::PamReturnCode::SUCCESS as libc::c_int
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        println!("Usage: test_pam <username> <password>");
        return;
    }
    let username = &args[1];
    let password = &args[2];

    println!("Starting test PAM for user: {}", username);

    let mut handle: *mut pam_sys::PamHandle = ptr::null_mut();
    let data = Box::new(PamSessionData {
        username: username.to_string(),
        password: password.to_string(),
    });

    let conv = pam_sys::PamConversation {
        conv: Some(pam_conversation_fn),
        data_ptr: &*data as *const PamSessionData as *mut libc::c_void,
    };

    unsafe {
        let service = "cce-display-manager";
        let rc = pam_sys::start(service, Some(username), &conv, &mut handle);
        if rc != pam_sys::PamReturnCode::SUCCESS {
            println!("pam_start failed: {:?}", rc);
            return;
        }

        let pass_c = CString::new(password.clone()).unwrap();
        pam_sys::raw::pam_set_item(handle, pam_sys::PamItemType::AUTHTOK as libc::c_int, pass_c.as_ptr() as *const libc::c_void);

        let tty_c = CString::new("tty1").unwrap();
        pam_sys::raw::pam_set_item(handle, pam_sys::PamItemType::TTY as libc::c_int, tty_c.as_ptr() as *const libc::c_void);

        let rc = pam_sys::authenticate(&mut *handle, pam_sys::PamFlag::NONE);
        if rc != pam_sys::PamReturnCode::SUCCESS {
            println!("pam_authenticate failed: {:?}", rc);
            return;
        }

        let rc = pam_sys::acct_mgmt(&mut *handle, pam_sys::PamFlag::NONE);
        if rc != pam_sys::PamReturnCode::SUCCESS {
            println!("pam_acct_mgmt failed: {:?}", rc);
            return;
        }

        let rc = pam_sys::setcred(&mut *handle, pam_sys::PamFlag::ESTABLISH_CRED);
        if rc != pam_sys::PamReturnCode::SUCCESS {
            println!("pam_setcred (establish) failed: {:?}", rc);
            return;
        }

        let rc = pam_sys::open_session(&mut *handle, pam_sys::PamFlag::NONE);
        if rc != pam_sys::PamReturnCode::SUCCESS {
            println!("pam_open_session failed: {:?}", rc);
            return;
        }

        println!("PAM Session opened successfully!");

        let env_list = pam_sys::getenvlist(&mut *handle);
        if !env_list.is_null() {
            let mut idx = 0;
            loop {
                let env_ptr = *env_list.offset(idx);
                if !env_ptr.is_null() {
                    idx += 1;
                    let env_str = CStr::from_ptr(env_ptr).to_string_lossy();
                    println!("PAM_ENV: {}", env_str);
                } else {
                    break;
                }
            }
            pam_sys::raw::pam_misc_drop_env(env_list as *mut *mut libc::c_char);
        } else {
            println!("pam_getenvlist returned NULL");
        }

        // Clean up
        pam_sys::close_session(&mut *handle, pam_sys::PamFlag::NONE);
        pam_sys::setcred(&mut *handle, pam_sys::PamFlag::DELETE_CRED);
        pam_sys::end(&mut *handle, pam_sys::PamReturnCode::SUCCESS);
    }
}
