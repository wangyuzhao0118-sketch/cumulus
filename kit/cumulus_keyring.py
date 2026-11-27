"""Cumulus 多账号：colab CLI 的令牌照样存在 macOS 钥匙串，按 CUMULUS_ACCOUNT 分开条目。"""
import os

from keyring.backends.macOS import Keyring as _Mac

_SERVICE = "google-colab-cli"


def _user(service, username):
    acct = os.environ.get("CUMULUS_ACCOUNT", "")
    if acct and acct != "default" and service == _SERVICE:
        return f"{username}@cumulus:{acct}"
    return username


class Keyring(_Mac):
    def get_password(self, service, username):
        return super().get_password(service, _user(service, username))

    def set_password(self, service, username, password):
        return super().set_password(service, _user(service, username), password)

    def delete_password(self, service, username):
        return super().delete_password(service, _user(service, username))
