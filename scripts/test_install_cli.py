import pathlib
import unittest

SCRIPT = pathlib.Path(__file__).with_name("install-cli.sh")


class InstallCliTextTests(unittest.TestCase):
    def test_next_steps_match_the_current_cli(self):
        text = SCRIPT.read_text(encoding="utf-8")
        self.assertIn("cayw conf", text)
        self.assertIn("cayw start --ui tui            # 启动终端界面", text)
        self.assertIn("cayw start --ui web", text)
        self.assertIn("启动不会弹出配置向导", text)
        self.assertIn("无模型也能启动并创建/查询会话", text)
        self.assertNotIn("cayw configure", text)
        self.assertNotIn("只保存凭据的环境变量名", text)
        self.assertNotIn("统一启动当前工作区及网页界面", text)


if __name__ == "__main__":
    unittest.main()
