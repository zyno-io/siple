# Transfer a call

Pass a caller to a colleague, either straight away or after you have spoken to them first.

<Shot name="transfer-composer" alt="The transfer composer open on an active call, with extension suggestions" caption="The transfer composer: type a number or pick an extension." />

## Open the transfer panel

1. During a call, choose **Transfer**, or press <Keys combo="Mod+Shift+T" />.
2. Type a number or SIP address into the **Number or SIP URI** field, or pick an extension from the suggestions below it.
3. Choose **Consult first** or **Transfer now**, as described below.

To back out, choose **Close**. The caller you are transferring is called A, and the person you are transferring to is B.

## Send the call straight away (blind transfer)

Choose **Transfer now**. Siple sends the caller to the target immediately, and the call ends on your side.

The buttons show which key does what. By default <Keys combo="Enter" /> starts a consult and <Keys combo="Mod+Enter" /> sends
the call straight away.

::: details In config.toml
Set `[ui] transfer_enter = "blind"` to make <Keys combo="Enter" /> a blind transfer instead, and <Keys combo="Mod+Enter" />
a consult.
:::

## Talk first (attended transfer)

<Shot name="transfer-consult" alt="An attended transfer with a consult call" caption="Speaking to the colleague before completing the transfer." />

1. Choose **Consult first**. Siple puts the original caller (A) on hold and calls the target (B).
2. Speak to B. Choose **Swap to** (followed by a name) to go back and forth between A and B.
3. When B agrees, choose **Complete transfer**. A is connected to B and your lines close.

If you change your mind, choose **Cancel - return to** the original caller. The consult call ends and A is back on the line.

What you can do at each stage:

| Stage | What you can do |
|---|---|
| Consult dialing or ringing | **Transfer now (blind)** or **Cancel - return to** A. **Complete transfer** becomes available when B answers. |
| Consult connected | **Complete transfer** (A to B), **Swap to** A or B to speak to either, or **Cancel**. |
| Consult failed | **Cancel - return to** A. |

## If a transfer fails

When a transfer fails for good, Siple takes the original call off hold again, so you are back with the caller.

A transfer can also end with no answer from the phone system. If nothing comes back after 32 s, Siple is never sure whether
the call went through, so it does not resume the original call on its own. It shows **Transfer unconfirmed** and offers
**Resume** or **Hang up**, each followed by the original caller's name.

::: details In config.toml
`[calls] resume_after_failed_transfer` (on by default) takes the original call off hold after a transfer that fails for good.
In **Settings › Calls & lines** it is **Resume the held call after a failed transfer**. A transfer with no result after 32 s
is never resumed automatically.
:::

## When someone transfers you

You can also be the one who is transferred. When the far end transfers you, the call follows to the new person.

## Details

Transfer sessions live in the engine, so the same session works for the GUI, the API and agents. Details of every state are in
the [API reference](/agents/api-reference).

Next: [See who is free](/guide/extensions).
