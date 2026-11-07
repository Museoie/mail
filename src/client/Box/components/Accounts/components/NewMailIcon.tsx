import { SVGProps } from "react";

// Original drawing (not FontAwesome): an envelope — a filled rect with a
// shaded V flap. The flap is black at low opacity so it reads against the
// rect on any rail background. Used for the "New" rail item.
const NewMailIcon = (props: SVGProps<SVGSVGElement>) => (
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
      d="M64 112 h384 c17.7 0 32 14.3 32 32 v224 c0 17.7-14.3 32-32 32 H64 c-17.7 0-32-14.3-32-32 V144 c0-17.7 14.3-32 32-32 z"
    ></path>
    <path
      fill="#000"
      opacity="0.35"
      d="M48 112 L256 288 L464 112 L464 96 H48 Z"
    ></path>
  </svg>
);

export default NewMailIcon;
