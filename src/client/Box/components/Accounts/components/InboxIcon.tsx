import { SVGProps } from "react";

// Original drawing (not FontAwesome): an inbox tray — rim bar over a
// trapezoid body. Used for the "All" rail item.
const InboxIcon = (props: SVGProps<SVGSVGElement>) => (
  <svg
    aria-hidden="true"
    focusable="false"
    role="img"
    xmlns="http://www.w3.org/2000/svg"
    viewBox="0 0 512 512"
    {...props}
  >
    <path
      fill="currentColor"
      d="M32 160 V128 a32 32 0 0 1 32-32 h384 a32 32 0 0 1 32 32 v32 z"
    ></path>
    <path
      fill="currentColor"
      d="M80 192 h352 l-24 200 a48 48 0 0 1-48 48 H152 a48 48 0 0 1-48-48 z"
    ></path>
  </svg>
);

export default InboxIcon;
