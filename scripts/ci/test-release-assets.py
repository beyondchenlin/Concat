#!/usr/bin/env python3
"""Check exact product filenames and draft upload evidence without a network call."""

import copy
import importlib.util
import pathlib
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "models.py"
SPEC = importlib.util.spec_from_file_location("product_models", SCRIPT)
models = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(models)


class ReleaseAssetsTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        bundles = pathlib.Path(self.directory.name)
        for index, (_, _, _, stem) in enumerate(models.BUNDLES):
            (bundles / stem.format(v="0.2.13")).write_bytes(f"test bundle {index}".encode())
        original_table = models.table
        models.table = lambda: {"release": {"tag": "fixture"}, "model": []}
        self.addCleanup(setattr, models, "table", original_table)
        self.manifest = models.release_manifest("0.2.13", "v0.2.13", bundles)
        self.release = {"tag_name": "v0.2.13", "draft": True}
        self.assets = []
        for architectures in self.manifest["binaries"].values():
            for kinds in architectures.values():
                for entry in kinds.values():
                    self.assertTrue(entry["file"].startswith("BK CUT-"))
                    self.assertEqual(entry["file"].count(" "), 1)
                    self.assertIn("/BK%20CUT-", entry["url"])
                    self.assets.append({
                        "name": entry["file"], "browser_download_url": entry["url"],
                        "size": entry["bytes"], "digest": "sha256:" + entry["sha256"],
                        "state": "uploaded",
                    })

    def verify(self, assets=None, release=None):
        assets = self.assets if assets is None else assets
        return models.verify_release_assets(
            self.manifest, self.release if release is None else release,
            [assets[:5], assets[5:]],
        )

    def test_exact_files_and_urls_across_all_pages(self):
        self.assertEqual(self.verify(), len(models.BUNDLES))

    def test_normalized_names_and_unencoded_urls_are_refused(self):
        for key, value in [
            ("name", self.assets[0]["name"].replace("BK CUT-", "BK.CUT-")),
            ("name", self.assets[0]["name"].replace("BK CUT-", "BK-CUT-")),
            ("browser_download_url", self.assets[0]["browser_download_url"].replace("%20", " ")),
            ("browser_download_url", self.assets[0]["browser_download_url"].replace("%20", "+")),
            ("size", 99999), ("state", "new"), ("digest", "sha256:" + "0" * 64),
        ]:
            with self.subTest(key=key, value=value):
                assets = copy.deepcopy(self.assets)
                assets[0][key] = value
                with self.assertRaises(ValueError):
                    self.verify(assets=assets)

    def test_missing_or_duplicate_assets_are_refused(self):
        for assets in [self.assets[:-1], self.assets + [self.assets[0]]]:
            with self.assertRaises(ValueError):
                self.verify(assets=assets)

    def test_published_or_different_release_is_refused(self):
        for release in [
            {"tag_name": "v0.2.13", "draft": False},
            {"tag_name": "v0.2.14", "draft": True},
        ]:
            with self.assertRaises(ValueError):
                self.verify(release=release)


if __name__ == "__main__":
    unittest.main()
