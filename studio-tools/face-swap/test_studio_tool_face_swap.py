import importlib.util
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("studio-tool-face-swap.py")
LAUNCHER = Path(__file__).with_name("studio-tool-face-swap")
SPEC = importlib.util.spec_from_file_location("studio_tool_face_swap", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class FaceSwapToolContractTests(unittest.TestCase):
    def test_launcher_bootstraps_and_caches_python_dependencies(self):
        launcher = LAUNCHER.read_text(encoding="utf-8")
        self.assertIn('"$SYSTEM_PYTHON" -m venv "$VENV"', launcher)
        self.assertIn('"$PYTHON" -m pip install', launcher)
        self.assertIn('STAMP="$VENV/.requirements.sha256"', launcher)
        self.assertIn("LITTLE_MONKEY_STUDIO_TOOL_DATA_DIR", launcher)
        self.assertNotIn("$SCRIPT_DIR/.face-swap-venv", launcher)

    def test_manifest_declares_the_studio_face_swap_inputs(self):
        self.assertEqual(MODULE.MANIFEST["schemaVersion"], 1)
        self.assertEqual(MODULE.MANIFEST["id"], "ghost-face-swap-local")
        self.assertFalse(MODULE.MANIFEST["licenseNotice"]["commercialUseAllowed"])
        self.assertIn("non-commercial", MODULE.MANIFEST["licenseNotice"]["title"].lower())
        self.assertEqual(
            [input_["key"] for input_ in MODULE.MANIFEST["inputs"]],
            [
                "source",
                "target",
                "face_swap_model",
                "restorer",
                "codeformer_weight",
                "license_acknowledged",
                "source_face_index",
                "target_face_index",
                "swap_all",
                "face_swap_weight",
            ],
        )

    def test_loopback_guard_rejects_public_bindings(self):
        with self.assertRaises(MODULE.ToolError):
            MODULE.ensure_loopback("0.0.0.0")

    def test_face_index_validation_is_whole_number_only(self):
        self.assertEqual(MODULE.FaceSwapRuntime._index(2, "Face"), 2)
        with self.assertRaises(MODULE.ToolError):
            MODULE.FaceSwapRuntime._index(1.5, "Face")
        with self.assertRaises(MODULE.ToolError):
            MODULE.FaceSwapRuntime._index(True, "Face")

    def test_face_analysis_pack_requires_downloads_to_be_present(self):
        with self.subTest("missing pack"):
            with self.assertRaises(MODULE.ToolError):
                MODULE.require_face_analysis_models(Path("/tmp/does-not-exist"), "buffalo_l")

    def test_public_model_downloads_are_pinned_and_https_only(self):
        downloads = [
            *MODULE.MODEL_DOWNLOADS.values(),
            MODULE.CODEFORMER_SOURCE_DOWNLOAD,
            MODULE.CODEFORMER_MODEL_DOWNLOAD,
        ]
        self.assertTrue(all(item["url"].startswith("https://") for item in downloads))
        self.assertTrue(all(len(item["sha256"]) == 64 for item in downloads))
        self.assertEqual(MODULE.CODEFORMER_MODEL_DOWNLOAD["url"].split("/")[-1], "codeformer.pth")

    def test_archive_path_guard_rejects_traversal(self):
        with self.assertRaises(MODULE.ToolError):
            MODULE._safe_archive_path(Path("/tmp/models"), "../../outside")

    def test_ghost_weight_validation(self):
        self.assertEqual(MODULE.FaceSwapRuntime._weight(0.5), 0.5)
        with self.assertRaises(MODULE.ToolError):
            MODULE.FaceSwapRuntime._weight(1.5)

    def test_model_and_restorer_choices_are_strict(self):
        self.assertEqual(MODULE.ghost_model_name(None), "ghost_3_256")
        self.assertEqual(MODULE.FaceSwapRuntime._restorer("none"), "none")
        with self.assertRaises(MODULE.ToolError):
            MODULE.ghost_model_name("ghost_1_256")
        with self.assertRaises(MODULE.ToolError):
            MODULE.FaceSwapRuntime._restorer("gfpgan")

    def test_license_acknowledgement_is_required_before_loading_models(self):
        with self.assertRaises(MODULE.ToolError):
            MODULE.FaceSwapRuntime().run({"license_acknowledged": False})


if __name__ == "__main__":
    unittest.main()
