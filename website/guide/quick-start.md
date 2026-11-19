# Quick start

From download to your first call in about five minutes.

## 1. Get your account details

Siple connects to your phone system with a **SIP account**. You need three things, which your IT team or phone provider can
give you:

| You need | Often called | Example |
|---|---|---|
| User name | Extension, SIP user, auth ID | `1001` |
| Password | SIP password, secret | (from your provider) |
| Server | Domain, registrar, SIP server, host | `pbx.example.com` |

::: tip Not sure which system you have?
Siple works with standard SIP phone systems such as Asterisk, FreePBX and FreeSWITCH, and with hosted SIP providers. If a
desk phone or another softphone can sign in with a user name, password and server, Siple can too.
:::

## 2. Install Siple

Download the build for your computer from [siple.app](https://siple.app/#download), which picks the right file for you.

<OsTabs>
<template #mac>

Open `Siple-…-macos.dmg` and drag **Siple** into **Applications**. Then open Siple from Applications or Spotlight.

</template>
<template #windows>

Unzip `Siple-…-windows-….zip` into a folder of your own, for example `%LOCALAPPDATA%\Programs\Siple`, and run `siple.exe`.
When Windows Defender Firewall asks about Siple, choose **Allow** for private and public networks, or you may not hear the
other person.

</template>
<template #linux>

```sh
tar xzf Siple-*-linux-x86_64.tar.gz
cd siple-*/
./bin/siple
```

</template>
</OsTabs>

More detail for each system, including the Linux packages Siple needs: [Install](/guide/install).

## 3. Add your account

The first time Siple starts, a short setup opens.

<Shot name="wizard-account" focus="314,0,612,428" alt="The setup's first step: display name, user name, password and server" />

1. Fill in **Display name** (what people see when you call), **Username / extension**, **Password** and **Domain / server**.
2. Choose **Test & continue**. Siple checks the details with your server before saving anything. If something is wrong, it
   says what, for example a wrong password, and nothing is saved until it works.
3. On the **Check your audio** step, pick your microphone and speaker. Talk and watch the level meter move, then try **Test speaker**
   and **Echo test** (it records three seconds and plays them back).
4. On the last step, choose whether Siple opens phone links from your browser, sits in the tray or menu bar, and shows
   notifications. Choose **Finish**.

When the account's lamp in the top bar turns **green**, you are ready.

## 4. Make a test call

Type a number into the **Number or SIP URI** field and press <Keys combo="Enter" /> or **Call**. Call a colleague, your own
mobile, or a test number from your provider. On many Asterisk and FreePBX systems `*43` is an echo test.

On macOS, allow microphone access when you are asked on your first call.

## Next steps

- [Make and answer calls](/guide/calls): hold, mute, call waiting and several lines at once.
- [Transfer a call](/guide/transfers) to a colleague.
- [See who is free](/guide/extensions) with busy lamps.
- Something not working? [Troubleshooting](/guide/troubleshooting).
