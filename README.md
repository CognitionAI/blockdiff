# blockdiff

Fast block-level file diffs (e.g. for VM disk images) using CoW filesystem metadata

Check out the technical writeup [here](https://cognition.ai/blog/blockdiff).

## Usage

### File snapshots

Creating a snapshot:

```
blockdiff create output.bdiff target.img --base base.img // creates output.bdiff from target.img and base.img
```

Applying a snapshot:

```
blockdiff apply input.bdiff target.img --base base.img // creates target.img from input.bdiff and base.img
```

### Compactifying sparse files

You can also use the blockdiff tool without a base image. This can be used to "compactify" sparse files for uploading to storage. A sparse file might have a size of 100GB, but only 10GB of data. The blockdiff tool can create a compact 10GB blockdiff file that contains only the actual data. (Under the hood, it is equivalent to creating a blockdiff with an empty sparse file as the base.)

```
blockdiff create compact.bdiff target.img // consolidates sparse file into a new compact blockdiff 
blockdiff apply compact.bdiff target.img // creates a new sparse target.img from the blockdiff
```
