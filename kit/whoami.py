"""打印当前账号（CUMULUS_ACCOUNT）的 Google 邮箱。用 colab CLI 自己的 Python 和登录信息运行。"""
from colab_cli import auth

s = auth.get_credentials()
r = s.get("https://openidconnect.googleapis.com/v1/userinfo", timeout=20)
r.raise_for_status()
print("@@EMAIL " + r.json().get("email", ""))
