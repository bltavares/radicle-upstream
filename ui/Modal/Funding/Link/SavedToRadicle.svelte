<script lang="typescript">
  import { Button, Emoji } from "../../../DesignSystem/Primitive";

  export let onCancel: () => void;
  export let onContinue: () => void;
</script>

<style>
  .wrapper {
    display: flex;
    justify-content: space-around;
    align-items: center;
    flex-direction: column;
  }

  header {
    padding: 0 var(--content-padding);
  }

  .submit {
    display: flex;
    justify-content: flex-end;
    width: 100%;
    margin-top: 1.5rem;
  }
</style>

<div class="wrapper">
  <Emoji emoji="🧦" size="huge" />

  <header>
    <h1 style="margin-top: 1.5rem;">
      Your Ethereum address was saved to Radicle
    </h1>
    <p style="padding: 2rem 4rem 0.5rem 4rem" class="typo-text">
      Continue to add your Radicle Identity to Ethereum?
    </p>
  </header>

  <div class="submit">
    <Button variant="transparent" dataCy="cancel-topup" on:click={onCancel}>
      Cancel
    </Button>

    <Button
      dataCy="confirm-button"
      on:click={onContinue}
      style="margin-left: 14px;">
      Continue
    </Button>
  </div>
</div>
