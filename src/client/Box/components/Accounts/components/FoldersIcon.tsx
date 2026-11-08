import { SVGProps } from "react";

// Original drawing (not FontAwesome): a folder silhouette with a tab.
// Used for the narrow-nav "Folders" rail item.
const FoldersIcon = (props: SVGProps<SVGSVGElement>) => (
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
      d="M64 128 a32 32 0 0 1 32-32 h112 l48 48 h160 a32 32 0 0 1 32 32 v224 a32 32 0 0 1-32 32 H96 a32 32 0 0 1-32-32 z"
    ></path>
  </svg>
);

export default FoldersIcon;
