"""Root provisioning repairs only the discovered user's authorization objects."""
import errno
import importlib.util
import io
from pathlib import Path
import stat
import types
import unittest

ROOT = Path(__file__).resolve().parents[1]
IDENTITY = {"user": "operator", "uid": 1007, "gid": 1011, "home": "/home/operator"}
HOME = "/home/operator"
SSH = HOME + "/.ssh"
KEYS = SSH + "/authorized_keys"


def attrs(mode, uid=1007, gid=1011, links=1):
    return types.SimpleNamespace(st_mode=mode, st_uid=uid, st_gid=gid, st_nlink=links)


class SftpTree:
    def __init__(self, paths):
        self.paths = {"/home": attrs(stat.S_IFDIR | 0o755, 0, 0), **paths}
        self.content = b"ssh-rsa existing-key\n"
        self.changed = []
        self.ignore_chmod = False

    def lstat(self, path):
        if path not in self.paths:
            raise FileNotFoundError(errno.ENOENT, "synthetic missing")
        return types.SimpleNamespace(**vars(self.paths[path]))

    def mkdir(self, path, mode):
        if path in self.paths:
            raise FileExistsError(errno.EEXIST, "synthetic exists")
        if str(Path(path).parent).replace("\\", "/") not in self.paths:
            raise FileNotFoundError(errno.ENOENT, "missing parent")
        self.paths[path] = attrs(stat.S_IFDIR | mode, 0, 0)
        self.changed.append(("mkdir", path))

    def chmod(self, path, mode):
        self.changed.append(("chmod", path))
        if not self.ignore_chmod:
            self.paths[path].st_mode = stat.S_IFMT(self.paths[path].st_mode) | mode

    def chown(self, path, uid, gid):
        self.changed.append(("chown", path))
        self.paths[path].st_uid, self.paths[path].st_gid = uid, gid

    def open(self, path, mode):
        if "x" in mode:
            if path in self.paths:
                raise FileExistsError(errno.EEXIST, "synthetic exists")
            self.paths[path] = attrs(stat.S_IFREG | 0o600, 0, 0)
            self.content = b""
            self.changed.append(("create", path))
        else:
            self.lstat(path)
        outer = self
        class File(io.BytesIO):
            def close(self):
                if "a" in mode or "x" in mode:
                    outer.content = self.getvalue()
                    outer.changed.append(("write", path))
                super().close()
        stream = File(self.content)
        if "a" in mode:
            stream.seek(0, 2)
        return stream


class UserSetupTests(unittest.TestCase):
    def setUp(self):
        spec = importlib.util.spec_from_file_location("user_setup_provision", ROOT / "tools/paramiko_provision.py")
        self.helper = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(self.helper)

    def install(self, tree):
        return self.helper.install_public_key(tree, "ssh-rsa new-key", identity=IDENTITY)

    def test_missing_home_and_ssh_objects_are_created_automatically(self):
        tree = SftpTree({})
        self.assertEqual(self.install(tree), "APPENDED")
        self.assertEqual(tree.content, b"ssh-rsa new-key\n")
        for path, mode in ((HOME, 0o750), (SSH, 0o700), (KEYS, 0o600)):
            actual = tree.lstat(path)
            self.assertEqual((stat.S_IMODE(actual.st_mode), actual.st_uid, actual.st_gid), (mode, 1007, 1011))
        self.assertEqual({path for _, path in tree.changed}, {HOME, SSH, KEYS})

    def test_root_owned_or_writable_objects_are_repaired_without_losing_existing_keys(self):
        for uid in (0, 1007):
            with self.subTest(uid=uid):
                tree = SftpTree({HOME: attrs(stat.S_IFDIR | 0o777, uid, 0),
                                 SSH: attrs(stat.S_IFDIR | 0o775, uid, 0),
                                 KEYS: attrs(stat.S_IFREG | 0o666, uid, 0)})
                self.assertEqual(self.install(tree), "APPENDED")
                self.assertEqual(tree.content, b"ssh-rsa existing-key\nssh-rsa new-key\n")
                for path, mode in ((HOME, 0o755), (SSH, 0o700), (KEYS, 0o600)):
                    actual = tree.lstat(path)
                    self.assertEqual((stat.S_IMODE(actual.st_mode), actual.st_uid, actual.st_gid), (mode, 1007, 1011))
                changes = list(tree.changed)
                self.assertEqual(self.install(tree), "ALREADY_PRESENT")
                self.assertEqual(tree.changed, changes, "second preparation must not rewrite safe objects")

    def test_unsafe_types_and_other_owners_are_rejected_before_any_mutation(self):
        safe = {HOME: attrs(stat.S_IFDIR | 0o750), SSH: attrs(stat.S_IFDIR | 0o700),
                KEYS: attrs(stat.S_IFREG | 0o600)}
        variants = [(path, attrs(stat.S_IFLNK | 0o777)) for path in ("/home", HOME, SSH, KEYS)]
        variants += [(path, attrs(stat.S_IFDIR | 0o700, 1008, 1008)) for path in (HOME, SSH)]
        variants += [(KEYS, attrs(stat.S_IFREG | 0o600, 1008, 1008)),
                     (KEYS, attrs(stat.S_IFIFO | 0o600))]
        for path, value in variants:
            with self.subTest(path=path, attrs=vars(value)):
                tree = SftpTree({**safe, path: value})
                with self.assertRaises(ValueError):
                    self.install(tree)
                self.assertEqual(tree.changed, [])
                self.assertEqual(tree.content, b"ssh-rsa existing-key\n")

    def test_failed_mode_repair_cannot_report_key_installed(self):
        tree = SftpTree({HOME: attrs(stat.S_IFDIR | 0o777), SSH: attrs(stat.S_IFDIR | 0o700),
                         KEYS: attrs(stat.S_IFREG | 0o600)})
        tree.ignore_chmod = True
        with self.assertRaises(ValueError):
            self.install(tree)
        self.assertEqual(tree.content, b"ssh-rsa existing-key\n")

    def test_root_connection_failure_is_not_misreported_as_home_permission_failure(self):
        def denied(*args, **kwargs):
            raise OSError("synthetic auth failure")
        metadata = {"schema": "worker-provision/1", "host": "192.0.2.10", "root_user": "root",
                    "model_base_url": "https://model.example", "model": "test-model"}
        with self.assertRaises(self.helper.ProvisionStageError) as caught:
            self.helper.bootstrap_ordinary_access(metadata, b"synthetic-password", "known_hosts", connector=denied)
        self.assertEqual(caught.exception.error_type, "root_ssh_unavailable")
        self.assertNotIn("synthetic auth failure", caught.exception.next_step)


if __name__ == "__main__":
    unittest.main()
