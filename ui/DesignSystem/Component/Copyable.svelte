<script>
  // TODO(sarah): write tests for this once it's implemented in the ui somewhere
  import { copyToClipboard } from "../../../native/ipc.js";

  let slotContent;

  const copy = () => {
    copyToClipboard(slotContent.textContent.trim());
    afterCopy();
  };

  export let afterCopy = () => {};
</script>

<style>
  .container {
    cursor: pointer;
  }
</style>

<div class="container" on:click={copy}>
  <span bind:this={slotContent}>
    <slot />
  </span>
</div>