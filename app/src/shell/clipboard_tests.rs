use super::*;
use std::{cell::Cell, rc::Rc};

#[derive(Clone, Copy)]
enum Effect {
    BusyOnce,
    BusyAlways,
    Denied,
    Missing,
    Malformed,
    NonFiles,
    VirtualItems,
    PathsBusy,
}

// Fail after the paths were obtained, so accepting partial data is detectable.
#[windows::core::implement(IDataObject)]
struct Source {
    generation: Rc<Cell<usize>>,
    effect: Effect,
}
impl IDataObject_Impl for Source_Impl {
    fn GetData(&self, requested: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        // SAFETY: IDataObject::GetData receives a live FORMATETC for this callback.
        let cf = unsafe { (*requested).cfFormat };
        let bytes = if cf == CF_HDROP.0 {
            if matches!(self.effect, Effect::NonFiles | Effect::VirtualItems) {
                return Err(DV_E_FORMATETC.into());
            }
            if matches!(self.effect, Effect::PathsBusy) {
                return Err(CLIPBRD_E_CANT_OPEN.into());
            }
            let generation = self.generation.get() + 1;
            self.generation.set(generation);
            hdrop_bytes(&[PathBuf::from(format!(r"C:\attempt-{generation}.txt"))]).unwrap()
        } else if cf as u32 == preferred_effect() {
            match self.effect {
                Effect::BusyOnce if self.generation.get() == 1 => {
                    return Err(CLIPBRD_E_CANT_OPEN.into());
                }
                Effect::BusyAlways => return Err(CLIPBRD_E_CANT_OPEN.into()),
                Effect::Denied => return Err(E_ACCESSDENIED.into()),
                Effect::Missing | Effect::NonFiles => return Err(DV_E_FORMATETC.into()),
                Effect::Malformed => vec![2],
                _ => DROPEFFECT_MOVE.0.to_le_bytes().to_vec(),
            }
        } else {
            return Err(DV_E_FORMATETC.into());
        };
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 {
                hGlobal: global(&bytes).unwrap(),
            },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn QueryGetData(&self, requested: *const FORMATETC) -> HRESULT {
        // SAFETY: COM supplies a live descriptor for this call.
        if matches!(self.effect, Effect::VirtualItems)
            && unsafe { (*requested).cfFormat } as u32 == format(w!("Shell IDList Array"))
        {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, _: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }
    fn SetData(
        &self,
        _: *const FORMATETC,
        _: *const STGMEDIUM,
        _: windows::core::BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumFormatEtc(&self, _: u32) -> windows::core::Result<IEnumFORMATETC> {
        Err(E_NOTIMPL.into())
    }
    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: windows::core::Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(E_NOTIMPL.into())
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(E_NOTIMPL.into())
    }
}
fn source(effect: Effect) -> (IDataObject, Rc<Cell<usize>>) {
    let generation = Rc::new(Cell::new(0));
    (
        Source {
            generation: generation.clone(),
            effect,
        }
        .into(),
        generation,
    )
}

#[test]
fn effect_failure_discards_paths_and_retries_the_whole_read() {
    let _ole = Apartment::new().unwrap();
    let (data, generation) = source(Effect::BusyOnce);
    let (_, contents) =
        read_consistent(|| read_clipboard_object(data.clone()), || 7, || {}).unwrap();
    assert_eq!(generation.get(), 2);
    assert_eq!(
        contents.paths.unwrap(),
        [PathBuf::from(r"C:\attempt-2.txt")]
    );
    assert!(is_cut(contents.effect));
}

#[test]
fn exhausted_or_permanent_failure_never_becomes_copy() {
    let _ole = Apartment::new().unwrap();
    for (effect, expected_reads) in [
        (Effect::BusyAlways, CLIPBOARD_READ_ATTEMPTS),
        (Effect::Denied, 1),
        (Effect::Malformed, 1),
    ] {
        let (data, generation) = source(effect);
        let pauses = Cell::new(0);
        let result = read_consistent(
            || read_clipboard_object(data.clone()),
            || 7,
            || pauses.set(pauses.get() + 1),
        );
        assert!(result.is_err());
        assert_eq!(generation.get(), expected_reads);
        assert_eq!(pauses.get(), expected_reads - 1);
    }
}

#[test]
fn genuinely_missing_effect_can_use_copy_default() {
    let _ole = Apartment::new().unwrap();
    let (data, _) = source(Effect::Missing);
    let contents = read_clipboard_object(data).unwrap();
    assert_eq!(contents.effect, DROPEFFECT_COPY.0);
    assert!(!is_cut(contents.effect));
}

#[test]
fn non_file_content_and_virtual_items_are_not_access_failures() {
    let _ole = Apartment::new().unwrap();
    let (text, _) = source(Effect::NonFiles);
    let contents = read_clipboard_object(text).unwrap();
    assert!(contents.paths.is_none() && !contents.virtual_items);

    let (virtual_items, _) = source(Effect::VirtualItems);
    let contents = read_clipboard_object(virtual_items.clone()).unwrap();
    assert!(contents.paths.is_none() && contents.virtual_items);
    assert!(is_cut(contents.effect));
    assert_eq!(contents.data.as_raw(), virtual_items.as_raw());

    let (busy, _) = source(Effect::PathsBusy);
    assert_eq!(hdrop_paths(&busy).unwrap_err().code(), CLIPBRD_E_CANT_OPEN);
}

#[test]
fn sequence_change_discards_a_successful_read() {
    let mut sequences = [10, 11, 11, 11].into_iter();
    let attempts = Cell::new(0);
    let result = read_consistent(
        || {
            attempts.set(attempts.get() + 1);
            Ok(attempts.get())
        },
        || sequences.next().unwrap(),
        || {},
    )
    .unwrap();
    assert_eq!(result, (11, 2));
}

#[test]
fn continuously_changing_clipboard_is_a_failure() {
    let mut sequence = 0;
    let mut reads = 0;
    let result = read_consistent(
        || {
            reads += 1;
            Ok(())
        },
        || {
            sequence += 1;
            sequence
        },
        || {},
    );
    assert!(result.is_err());
    assert_eq!(reads, CLIPBOARD_READ_ATTEMPTS);
}

#[test]
fn sequence_change_after_failure_also_restarts_the_read() {
    let mut sequences = [10, 11, 11, 11].into_iter();
    let mut attempts = 0;
    let result = read_consistent(
        || {
            attempts += 1;
            if attempts == 1 {
                Err(E_ACCESSDENIED.into())
            } else {
                Ok("new data")
            }
        },
        || sequences.next().unwrap(),
        || {},
    )
    .unwrap();
    assert_eq!(result, (11, "new data"));
}
