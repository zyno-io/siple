# Set up your account

Connect Siple to your phone system and check that your headset works.

A **SIP account** is the login Siple uses to reach your **PBX** (the phone system, or your hosted SIP provider). If you do not
have your user name, password and server yet, see [Quick start](/guide/quick-start#1-get-your-account-details).

On first start Siple writes an example `config.toml` (its sample account is disabled). When no enabled account exists, it
opens the setup. The setup has three steps: **Add your SIP account**, **Check your audio** and **You're ready to make calls**.

## Add your account

<Shot name="wizard-account" focus="314,0,612,428" alt="The setup's first step: display name, user name, password and server" caption="Step 1 of 3 asks for your account details." />

1. Fill in **Display name** (optional), **Username / extension**, **Password** and **Domain / server**.
2. To keep the password out of the settings file, turn on **Store in system keychain** (see
   [Store the password in the keychain](#store-the-password-in-the-keychain)).
3. Open **Advanced options** only if your IT team asks for it. It holds **Auth username**, **Outbound proxy**,
   **Transport**, **Verify TLS certificate**, **SRTP** (call encryption) and **Phone profile**.
4. Choose **Test & continue**. Siple signs in once on a private connection to check the details before anything is saved. The
   check does not disturb accounts that are already running.

If you prefer to edit the file yourself, choose **Skip, I'll edit config.toml**.

::: details In config.toml
The check is the `account.test` API method. It registers once on a private socket, so it never touches running accounts.
:::

## If the test fails

The setup shows a message that explains what went wrong. Behind each message is one of these results:

| Result | What it means |
|---|---|
| `ok` | Signed in. |
| `auth_failed` | The server answered 401, 403 or 407 after the password was sent: wrong user name or password. |
| `not_found` | 404: the user does not exist on the server. |
| `timeout` | No answer within 15 s by default. If the transport is UDP, try TCP or TLS. |
| `tls_untrusted` | The server certificate is not trusted. The result includes its SHA-256 fingerprint. |
| `dns_failed` | The server name did not resolve. |
| `network_unreachable` | No route to the server. |
| `other` | Another SIP status or a local failure. |

Nothing is saved until the test works.

### Trust a private certificate

If your PBX uses a self-signed or private certificate, the setup offers **Trust this server** after a `tls_untrusted` result.
It remembers that certificate and tests again. **Turn off verification (not recommended)** ignores all certificate errors, so
use it only as a last resort.

::: details In config.toml
**Trust this server** pins the certificate by writing `tls_fingerprint` to the account. **Turn off verification (not
recommended)** sets `tls_verify = false`, which ignores all certificate errors. You can also trust a certificate authority
with `tls_ca_file`. To get the fingerprint of a PEM file:

```sh
openssl x509 -in pbx.pem -noout -fingerprint -sha256
```
:::

## Check your audio

<Shot name="wizard-audio" focus="314,0,612,370" alt="The setup's audio step: microphone, speaker and ringer with a level meter" caption="Step 2 of 3 checks your microphone and speaker." />

1. Pick your **Microphone**, **Speaker** and **Ringer**.
2. Talk and watch the level meter move.
3. Choose **Test speaker** to hear a 1 s tone.
4. Choose **Echo test** to record 3 s and hear it played back.
5. Choose **Continue**.

On macOS this step shows **Allow microphone access** if Siple does not have it yet. Choose it and allow the request.

## Choose how Siple behaves

On the last step, turn on or off:

- **Open sip:, tel: and callto: links with Siple**
- **Show in the tray / menu bar**
- **Desktop notifications**

Each one is a setting you can change later. Choose **Finish**. When the account's lamp in the top bar turns green, you are
ready to [make a call](/guide/calls).

**Skip, I'll edit config.toml** and **Finish** both record that the setup has run (in `state.toml`). To see it again, choose **Run setup again** in
**Settings › Accounts**.

## Store the password in the keychain

By default the password is stored in `config.toml`, which is created with mode 0600 (readable only by you). To keep it in
your computer's credential store instead, turn on **Store in system keychain** in the account form. In **Settings ›
Accounts** the switch is called **Store password in system keychain**.

The credential store is the macOS Keychain, Windows Credential Manager, or the Linux Secret Service (GNOME Keyring, KWallet).
To change existing accounts, use **Move passwords to keychain** or **Move passwords back to the config file** in **Settings ›
Accounts**.

::: warning No credential store available
If there is no credential store (headless Linux, Docker), the account is switched off with an error. Siple never falls back
to a plaintext password.
:::

::: details In config.toml
This is `password_store = "keyring"`. With the keychain, `password` in the file is empty. Details:
[Passwords in the system keyring](/reference/configuration#passwords-in-the-system-keyring).
:::

## Change your accounts later

**Settings › Accounts** edits the same `config.toml` the setup wrote. Choose **New account** to add one.

<Shot name="settings-accounts" alt="Settings, Accounts page" caption="Settings › Accounts lists your accounts and their details." />

Next: [Make and answer calls](/guide/calls).
