# Make and answer calls

Place, answer, hold and mute calls, and handle more than one call at a time.

<Shot name="idle" alt="Siple idle: no lines in use" caption="Siple with no calls. Lines are on the left, extensions on the right." />

## Make a call

1. Type a number or SIP address (a **SIP URI**, such as `sip:1002@pbx.example.com`) into the **Number or SIP URI** field.
2. Choose **Call**.

You can also click an extension key to call that person (see [See who is free](/guide/extensions)). To jump to the field from
the keyboard, press <Keys combo="Mod+L" />. When a call is already on screen, choose **New call** to open the field.

During a call, choose **Keypad** to send keypad tones (**DTMF**), for example to use a phone menu. You can also type digits,
`*` and `#` while a line is active and no field has focus.

## Answer a call

<Shot name="incoming" alt="An incoming call with Answer, Reject and Silence buttons" caption="An incoming call on desk-1001." />

An incoming call shows three buttons:

- **Answer** picks up. Keyboard: <Keys combo="Mod+Shift+A" />.
- **Reject** turns the caller away. It is called **Decline** while another call is live (see
  [Answer a second call](#answer-a-second-call)).
- **Silence** hides the ringing banner. The call keeps ringing until it is answered, declined or ends.

A call you did not answer is listed as **Missed**, with a **Call back** button.

::: details In config.toml
**Reject** sends 486 (Busy). While another call is live, **Decline** sends the code in `[calls] decline_code` instead (400 to
699, default 486). You can also change it in **Settings › Calls & lines** under **Decline code**.
:::

## Put a call on hold

Choose **Hold**. The same button then reads **Resume**. Keyboard: <Keys combo="Mod+Shift+H" />.

Resuming swaps lines, so you never have two live calls.

## Mute yourself

Choose **Mute**. The same button then reads **Unmute**. Keyboard: <Keys combo="Mod+Shift+M" />. A **Muted** tag shows on the
call while you are muted.

## End a call

Choose **Hang up**. Keyboard: <Keys combo="Mod+Shift+E" />, which ends the selected line.

## Switch between lines

Siple handles several calls at once, on one or more accounts. Each call has a line (L1, L2 and so on) in the column on the
left.

- Click a line to select it. Selecting a held line makes it the active one.
- Keyboard: <Keys combo="Mod+1" /> to <Keys combo="Mod+9" /> pick a line. <Keys combo="Mod+[" /> and <Keys combo="Mod+]" />
  go to the previous and next line.

Only one call that uses your headset is live at a time. When you answer, dial or select another call, the others are put on
hold automatically. Calls that use no audio device (agent calls) are exempt.

::: details In config.toml
`[calls] hold_scope` limits the automatic hold to one account. In **Settings › Calls & lines** it is **Hold scope**, with
**All accounts** or **Same account only**. Per account, `auto_hold = false` opts out. In **Settings › Accounts** that is the
switch **Hold other calls when answering**.
:::

## Answer a second call

<Shot name="call-waiting" alt="A second call ringing while a call is active" caption="A second call arrives during an active call." />

When a call comes in during another, the incoming card offers:

- **Answer & hold** (followed by the current caller's name): answers the new call and puts the current one on hold. This is
  the safe choice.
- **Answer & end** (followed by the current caller's name): ends the current call and answers the new one.
- **Decline** and **Silence**.

Press <Keys combo="Mod+Shift+A" /> to answer the selected ringing call, or the newest ringing one if none is selected. With another
line live, it answers the new call and holds the current one.

## Do not disturb and auto-answer

Do not disturb turns away incoming calls on an account. Click the account's name in the top bar and switch on **Do not
disturb**, or use **Settings › Accounts**. The switch lasts until Siple restarts; to keep it on, set `dnd = true` for the
account in `config.toml`. In **Settings › Accounts** you also find **Auto-answer** (with **Auto-answer delay
(ms)**) and **Allow intercom** for intercom calls.

::: details In config.toml
Per account: `dnd` (the lasting setting; the switch overrides it until the next restart or until `dnd` changes), `auto_answer` (with `answer_delay_ms`, and `allow_intercom` for intercom calls). See the
[`[[account]]` reference](/reference/configuration).
:::

Next: [Transfer a call](/guide/transfers).
