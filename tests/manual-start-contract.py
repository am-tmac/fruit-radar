"""Offline application wiring regression; paired with real Watcher startup_order.rs."""
import pathlib
import unittest
ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = (ROOT / "src-tauri/src/lib.rs").read_text()

class ManualStartContract(unittest.TestCase):
    def test_setup_initializes_without_reading_resume_or_starting(self):
        setup = SOURCE.split(".setup(|app| {", 1)[1].split(".on_window_event(", 1)[0]
        self.assertNotIn("watcher.start()", setup)
        self.assertNotIn("resume::read", setup)
        self.assertNotIn("resume_state_path", setup)
        self.assertLess(setup.index("watcher.set_targets(targets).await"), setup.index("watcher.set_interval(interval).await"))
        self.assertLess(setup.index("watcher.is_running().await"), setup.index("app.manage(AppState"))
        self.assertIn("tauri::async_runtime::block_on", setup)

    def test_manual_commands_keep_control_lock_and_stop_cancellation(self):
        start = SOURCE.split("async fn start_watching(", 1)[1].split("#[tauri::command]", 1)[0]
        stop = SOURCE.split("async fn stop_watching(", 1)[1].split("fn remember_running", 1)[0]
        self.assertLess(start.index("state.control.lock().await"), start.index("state.watcher.start().await"))
        self.assertLess(stop.index("state.control.lock().await"), stop.index("state.admission.lock().unwrap().cancel()"))
        self.assertLess(stop.index("state.admission.lock().unwrap().cancel()"), stop.index("state.watcher.stop().await"))

if __name__ == "__main__":
    unittest.main()
