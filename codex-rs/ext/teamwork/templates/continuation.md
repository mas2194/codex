The `/teamwork` run is still active, so the harness has resumed your work automatically.

<teamwork_state>
{{status}}
</teamwork_state>

<next_action>
{{next_action}}
</next_action>

Work from the current repository and the harness state, not from memory. If sub-agents are still running, wait for them with the wait tool before taking other actions. A turn that changes nothing in the harness state counts as no progress. After {{max_idle}} such turns in a row, the harness blocks the run for the user. If you genuinely need a user decision, call `teamwork_block` with a specific question instead of ending the turn.
