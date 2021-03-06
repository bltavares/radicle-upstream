import { derived, get, writable } from "svelte/store";

type OnHide = () => void;
const doNothing = () => {
  /**/
};

type ModalOverlay =
  | { show: true; route: string; onHide: OnHide }
  | { show: false };

const overlayStore = writable<ModalOverlay>({
  show: false,
});
export const store = derived(overlayStore, $store => $store);

export const hide = (): void => {
  const modal = get(store);
  if (modal.show) {
    modal.onHide();
  }

  overlayStore.set({ show: false });
};

export const toggle = (route: string, onHide: OnHide = doNothing): void => {
  const modal = get(store);
  if (modal.show && modal.route === route) {
    hide();
    return;
  }

  overlayStore.set({ show: true, route, onHide });
};
